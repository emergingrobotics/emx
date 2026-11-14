use slotmap::SlotMap;

use crate::buffer::BufferId;
use crate::marker::MarkerId;

slotmap::new_key_type! {
    pub struct WindowId;
}

/// `point` and `start` are markers in `buffer` created without undo
/// tracking, since Emacs window point and start are not undo-recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub buffer: BufferId,
    pub point: MarkerId,
    pub start: MarkerId,
}

/// One client's windows. Windows and point are per client; buffers are
/// shared between clients.
#[derive(Debug, Clone)]
pub struct Frame {
    pub selected: WindowId,
    pub windows: SlotMap<WindowId, Window>,
}

impl Frame {
    pub fn new(window: Window) -> Frame {
        let mut windows = SlotMap::with_key();
        let selected = windows.insert(window);
        Frame { selected, windows }
    }

    pub fn selected_window(&self) -> &Window {
        &self.windows[self.selected]
    }
}
