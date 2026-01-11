//! Macro execution engine

use std::process::{Command, Stdio};

use anyhow::Result;

use crate::config::{HotkeyType, Macro};
use crate::uinput::VirtualKeyboard;

pub struct MacroExecutor {
    keyboard: VirtualKeyboard,
}

impl MacroExecutor {
    pub fn new() -> Result<Self> {
        let keyboard = VirtualKeyboard::new()?;
        Ok(Self { keyboard })
    }

    /// Execute a macro based on its type
    pub fn execute(&mut self, macro_def: &Macro) -> Result<()> {
        match macro_def.hotkey_type {
            HotkeyType::Run => self.run_command(&macro_def.action),
            HotkeyType::Shortcut => self.keyboard.shortcut(&macro_def.action),
            HotkeyType::Typeout => self.keyboard.typeout(&macro_def.action),
            HotkeyType::Uinput => self.emit_uinput_key(&macro_def.action),
            HotkeyType::Sequence => self.keyboard.sequence(&macro_def.action),
            HotkeyType::Nothing => Ok(()),
        }
    }

    /// Run a shell command
    fn run_command(&self, cmd: &str) -> Result<()> {
        log::debug!("Running command: {}", cmd);
        Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }

    /// Emit a direct uinput key (e.g., "KEY_F13")
    fn emit_uinput_key(&mut self, key_name: &str) -> Result<()> {
        if let Some(key) = VirtualKeyboard::parse_key(key_name) {
            self.keyboard.click(key)?;
        } else {
            log::warn!("Unknown uinput key: {}", key_name);
        }
        Ok(())
    }
}
