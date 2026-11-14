//! Emacs registers and the one-character register-name read the `C-x r`
//! commands need. The command in `command::register` captures what to do and
//! calls `begin_*`; the editor reads the name key and carries it out.

use std::ops::Range;

use emx_types::{Key, KeyCode, Mods};

use super::Editor;
use crate::buffer::BufferId;
use crate::command::{self, CommandError};
use crate::event::ClientId;
use crate::marker::{InsertionType, MarkerId};

/// What a register holds; only text and point registers are needed here.
#[derive(Debug, Clone)]
pub(crate) enum RegisterValue {
    Text(Vec<u8>),
    Point { buffer: BufferId, marker: MarkerId },
}

/// The register operation waiting for its name character.
#[derive(Debug)]
pub(crate) enum RegisterRead {
    Copy { range: Range<u64>, delete: bool },
    Insert { exchange: bool },
    PointTo { pos: u64 },
    Jump,
}

const COPY_PROMPT: &str = "Copy to register: ";
const INSERT_PROMPT: &str = "Insert register: ";
const POINT_PROMPT: &str = "Point to register: ";
const JUMP_PROMPT: &str = "Jump to register: ";
const NO_TEXT: &str = "Register does not contain text";
const NO_POSITION: &str = "Register doesn't contain a buffer position or configuration";
const JUMP_TO_REGISTER: &str = "jump-to-register";

impl Editor {
    pub(crate) fn begin_copy_to_register(
        &mut self,
        client_id: ClientId,
        range: Range<u64>,
        delete: bool,
    ) {
        self.start_register_read(client_id, RegisterRead::Copy { range, delete }, COPY_PROMPT);
    }

    pub(crate) fn begin_insert_register(&mut self, client_id: ClientId, exchange: bool) {
        self.start_register_read(client_id, RegisterRead::Insert { exchange }, INSERT_PROMPT);
    }

    pub(crate) fn begin_point_to_register(&mut self, client_id: ClientId, pos: u64) {
        self.start_register_read(client_id, RegisterRead::PointTo { pos }, POINT_PROMPT);
    }

    pub(crate) fn begin_jump_to_register(&mut self, client_id: ClientId) {
        self.start_register_read(client_id, RegisterRead::Jump, JUMP_PROMPT);
    }

    fn start_register_read(&mut self, client_id: ClientId, read: RegisterRead, prompt: &str) {
        let client = self.client_mut(client_id);
        client.register_read = Some(read);
        client.echo = prompt.to_string();
    }

    fn set_register(&mut self, name: char, value: RegisterValue) {
        self.registers.insert(name, value);
    }

    /// The key after a `C-x r` command names the register.
    pub(super) fn read_register_key(&mut self, client_id: ClientId, key: Key) {
        let read = self
            .client_mut(client_id)
            .register_read
            .take()
            .expect("a register read is active");
        if key == super::QUIT_KEY {
            self.abort_input(client_id);
            self.signal(client_id, None, &CommandError::Quit);
            return;
        }
        let Some(name) = register_name(key) else {
            self.client_mut(client_id).register_read = Some(read);
            self.ring_bell(client_id);
            return;
        };
        match read {
            RegisterRead::Copy { range, delete } => {
                self.register_copy(client_id, name, range, delete);
            }
            RegisterRead::Insert { exchange } => self.register_insert(client_id, name, exchange),
            RegisterRead::PointTo { pos } => self.register_point_to(client_id, name, pos),
            RegisterRead::Jump => self.register_jump(client_id, name),
        }
    }

    fn register_copy(&mut self, client_id: ClientId, name: char, range: Range<u64>, delete: bool) {
        self.run_command(client_id, "copy-to-register", None, move |ctx| {
            let mut text = Vec::new();
            ctx.buffer().text.read(range.clone(), &mut text)?;
            ctx.editor().set_register(name, RegisterValue::Text(text));
            if delete {
                ctx.delete_with_point(range.clone(), range.start)?;
            }
            ctx.request_deactivate_mark();
            Ok(())
        });
    }

    fn register_insert(&mut self, client_id: ClientId, name: char, exchange: bool) {
        let value = self.registers.get(&name).cloned();
        self.run_command(client_id, "insert-register", None, move |ctx| {
            let Some(RegisterValue::Text(text)) = value else {
                return Err(CommandError::User(NO_TEXT.to_string()));
            };
            ctx.check_editable()?;
            let point = ctx.point();
            command::push_mark(ctx, point, true, false);
            ctx.insert(&text)?;
            if exchange {
                let mark = ctx.mark().expect("push-mark set the mark");
                let now = ctx.point();
                ctx.set_mark_position(now);
                ctx.set_point(mark);
            }
            Ok(())
        });
    }

    fn register_point_to(&mut self, client_id: ClientId, name: char, pos: u64) {
        if let Some(RegisterValue::Point { buffer, marker }) = self.registers.get(&name) {
            let (buffer, marker) = (*buffer, *marker);
            if self.buffers.contains_key(buffer) {
                self.buffers[buffer].delete_marker(marker);
            }
        }
        let buffer = self.selected_buffer(client_id);
        let marker = self.buffers[buffer].create_marker(pos, InsertionType::StayBefore);
        self.set_register(name, RegisterValue::Point { buffer, marker });
    }

    fn register_jump(&mut self, client_id: ClientId, name: char) {
        let target = match self.registers.get(&name) {
            Some(RegisterValue::Point { buffer, marker }) => {
                Some((*buffer, self.buffers[*buffer].marker_pos(*marker)))
            }
            _ => None,
        };
        match target {
            Some((buffer, pos)) if buffer == self.selected_buffer(client_id) => {
                self.run_command(client_id, JUMP_TO_REGISTER, None, move |ctx| {
                    let point = ctx.point();
                    command::push_mark(ctx, point, true, false);
                    ctx.set_point(pos);
                    Ok(())
                });
            }
            // Jumping to a register in another buffer is out of scope here.
            Some(_) => {}
            None => self.signal(
                client_id,
                Some(JUMP_TO_REGISTER),
                &CommandError::User(NO_POSITION.to_string()),
            ),
        }
    }
}

fn register_name(key: Key) -> Option<char> {
    match key {
        Key {
            code: KeyCode::Char(character),
            mods,
        } if mods == Mods::default() => Some(character),
        _ => None,
    }
}
