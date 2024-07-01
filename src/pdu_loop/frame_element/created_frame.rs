use crate::{
    error::PduError,
    fmt,
    generate::write_packed,
    pdu_loop::{
        frame_element::{receiving_frame::ReceiveFrameFut, FrameBox, FrameElement, FrameState},
        frame_header::EthercatFrameHeader,
        pdu_flags::PduFlags,
        pdu_header::PduHeader,
    },
    Command, PduLoop,
};
use core::{ptr::NonNull, sync::atomic::AtomicU8, time::Duration};
use ethercrab_wire::{EtherCrabWireSized, EtherCrabWireWrite, EtherCrabWireWriteSized};

/// A frame in a freshly allocated state.
///
/// This typestate may only be created by
/// [`alloc_frame`](crate::pdu_loop::storage::PduStorageRef::alloc_frame).
#[derive(Debug)]
pub struct CreatedFrame<'sto> {
    inner: FrameBox<'sto>,
    pdu_count: u8,
}

impl<'sto> CreatedFrame<'sto> {
    pub(in crate::pdu_loop) fn claim_created(
        frame: NonNull<FrameElement<0>>,
        frame_index: u8,
        pdu_idx: &'sto AtomicU8,
        frame_data_len: usize,
    ) -> Result<Self, PduError> {
        let frame = unsafe { FrameElement::claim_created(frame, frame_index)? };

        let mut inner = FrameBox::new(frame, pdu_idx, frame_data_len);

        inner.init();

        Ok(Self {
            inner,
            pdu_count: 0,
        })
    }

    /// The frame has been initialised, filled with a data payload (if required), and is now ready
    /// to be sent.
    ///
    /// This method returns a future that should be fulfilled when a response to the sent frame is
    /// received.
    pub fn mark_sendable(
        mut self,
        pdu_loop: &'sto PduLoop<'sto>,
        timeout: Duration,
        retries: usize,
    ) -> ReceiveFrameFut<'sto> {
        EthercatFrameHeader::pdu(self.inner.pdu_payload_len() as u16)
            .pack_to_slice_unchecked(self.inner.ecat_frame_header_mut());

        self.inner.set_state(FrameState::Sendable);

        ReceiveFrameFut {
            frame: Some(self.inner),
            pdu_loop,
            timeout_timer: crate::timer_factory::timer(timeout),
            timeout,
            retries_left: retries,
        }
    }

    pub fn push_pdu(
        &mut self,
        command: Command,
        data: impl EtherCrabWireWrite,
        len_override: Option<u16>,
        more_follows: bool,
    ) -> Result<PduResponseHandle, PduError> {
        let data_length_usize =
            len_override.map_or(data.packed_len(), |l| usize::from(l).max(data.packed_len()));

        let flags = PduFlags::new(data_length_usize as u16, more_follows);

        // PDU header + data + working counter (space is required for the response value - we never
        // actually write it)
        let alloc_size = data_length_usize + PduHeader::PACKED_LEN + 2;

        let consumed = self.inner.pdu_payload_len();

        // Comprises PDU header, body, working counter
        let buf_range = consumed..(consumed + alloc_size);

        // Establish mapping between this PDU index and the Ethernet frame it's being put in
        let pdu_idx = self.inner.next_pdu_idx();

        fmt::trace!(
            "Write PDU {:#04x} into frame index {} ({}, {} bytes at {:?})",
            pdu_idx,
            self.inner.frame_index(),
            command,
            data_length_usize,
            buf_range
        );

        let pdu_buf = self
            .inner
            .pdu_buf_mut()
            .get_mut(buf_range.clone())
            .ok_or(PduError::TooLong)?;

        let header = PduHeader {
            command_code: command.code(),
            index: pdu_idx,
            command_raw: command.pack(),
            flags,
            irq: 0,
        };

        let pdu_buf = write_packed(header, pdu_buf);

        // Payload
        let _pdu_buf = write_packed(data, pdu_buf);

        // Next two bytes are working counter, but they are always zero on send (and the buffer is
        // zero-initialised) so there's nothing to do.

        // Don't need to check length here as we do that with `pdu_buf_mut().get_mut()` above.
        self.inner.add_pdu(alloc_size, pdu_idx);

        let index_in_frame = self.pdu_count;

        self.pdu_count += 1;

        Ok(PduResponseHandle {
            index_in_frame,
            pdu_idx,
            command_code: command.code(),
        })
    }
}

// SAFETY: This unsafe impl is required due to `FrameBox` containing a `NonNull`, however this impl
// is ok because FrameBox also holds the lifetime `'sto` of the backing store, which is where the
// `NonNull<FrameElement>` comes from.
//
// For example, if the backing storage is is `'static`, we can send things between threads. If it's
// not, the associated lifetime will prevent the framebox from being used in anything that requires
// a 'static bound.
unsafe impl<'sto> Send for CreatedFrame<'sto> {}

#[derive(Debug)]
pub struct PduResponseHandle {
    pub index_in_frame: u8,

    /// PDU wire index and command used to validate response match.
    pub pdu_idx: u8,
    pub command_code: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdu_loop::frame_element::{AtomicFrameState, FrameElement};
    use atomic_waker::AtomicWaker;
    use core::{cell::UnsafeCell, ptr::NonNull, sync::atomic::AtomicU8};

    #[test]
    fn too_long() {
        let _ = env_logger::builder().is_test(true).try_init();

        const BUF_LEN: usize = 16;

        let pdu_idx = AtomicU8::new(0);

        let frames = UnsafeCell::new([FrameElement {
            frame_index: 0xab,
            status: AtomicFrameState::new(FrameState::None),
            waker: AtomicWaker::default(),
            ethernet_frame: [0u8; BUF_LEN],
            pdu_payload_len: 0,
            first_pdu: None,
        }]);

        let mut created = CreatedFrame::claim_created(
            unsafe { NonNull::new_unchecked(frames.get().cast()) },
            0xab,
            &pdu_idx,
            BUF_LEN,
        )
        .expect("Claim created");

        let handle = created.push_pdu(
            Command::fpwr(0x1000, 0x0918).into(),
            [0xffu8; 9],
            None,
            false,
        );

        assert_eq!(handle.unwrap_err(), PduError::TooLong);
    }
}
