// SPDX-License-Identifier: AGPL-3.0-only
//! Frames from the socket wait here and are applied together once per animation frame
//! (PLAN §5.4). Frames are never merged or reordered: the live store needs every version in
//! order. The saving is that the screen is updated once per batch, however many frames came.

use fleet_protocol::live::LiveFrame;

/// Frames received since the last flush.
#[derive(Debug, Default)]
pub struct FrameQueue {
    frames: Vec<LiveFrame>,
    flush_scheduled: bool,
}

impl FrameQueue {
    /// Adds a frame. Returns `true` when the caller must now schedule a flush (the queue was
    /// idle); later frames of the same batch return `false`.
    pub fn push(&mut self, frame: LiveFrame) -> bool {
        self.frames.push(frame);
        !std::mem::replace(&mut self.flush_scheduled, true)
    }

    /// Takes the batch, in arrival order, and goes idle.
    pub fn take(&mut self) -> Vec<LiveFrame> {
        self.flush_scheduled = false;
        std::mem::take(&mut self.frames)
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::live::ResyncReason;
    use uuid::Uuid;

    use super::*;

    fn delta(version: u64) -> LiveFrame {
        LiveFrame::Delta {
            project_id: Uuid::nil(),
            attempt_id: Uuid::nil(),
            version,
            entities: vec![],
        }
    }

    fn versions(frames: &[LiveFrame]) -> Vec<u64> {
        frames
            .iter()
            .filter_map(|f| match f {
                LiveFrame::Delta { version, .. } => Some(*version),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_burst_asks_for_one_flush_and_keeps_every_frame_in_order() {
        let mut q = FrameQueue::default();
        assert!(q.push(delta(1)), "the first frame schedules the flush");
        assert!(!q.push(delta(2)));
        assert!(!q.push(LiveFrame::Resync {
            reason: ResyncReason::Lagged
        }));
        assert!(!q.push(delta(3)));
        let batch = q.take();
        assert_eq!(batch.len(), 4);
        assert_eq!(versions(&batch), [1, 2, 3]);
        assert!(matches!(batch[2], LiveFrame::Resync { .. }), "order kept");
    }

    #[test]
    fn after_a_flush_the_next_frame_schedules_again() {
        let mut q = FrameQueue::default();
        assert!(q.push(delta(1)));
        let _ = q.take();
        assert_eq!(q.take().len(), 0, "nothing arrived since the flush");
        assert!(q.push(delta(2)));
    }
}
