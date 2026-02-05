use crate::termwindow::PaneMode;
use mux::termwiztermtab::TermWizTerminal;
use termwiz::lineedit::{BasicHistory, History, LineEditor, LineEditorHost};
use termwiz::surface::Change;
use termwiz::terminal::Terminal;

struct ModeSelectorHost {
    history: BasicHistory,
}

impl ModeSelectorHost {
    fn new() -> Self {
        Self {
            history: BasicHistory::default(),
        }
    }
}

impl LineEditorHost for ModeSelectorHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }
}

pub fn select_mode(mut term: TermWizTerminal, current: PaneMode) -> anyhow::Result<PaneMode> {
    term.no_grab_mouse_in_raw_mode();
    term.render(&[
        Change::Text("Select pane mode (terminal/ai).\r\n".to_string()),
        Change::Text("Press Enter to keep the current mode.\r\n\r\n".to_string()),
    ])?;

    let mut host = ModeSelectorHost::new();
    let mut editor = LineEditor::new(&mut term);
    editor.set_prompt(&format!("Mode [{}]: ", current.as_str()));
    let line = editor.read_line(&mut host)?.unwrap_or_default();
    let choice = line.trim().to_lowercase();

    if choice.is_empty() {
        return Ok(current);
    }

    let selected = match choice.as_str() {
        "terminal" | "term" | "t" => PaneMode::Terminal,
        "ai" | "a" => PaneMode::Ai,
        _ => {
            term.render(&[Change::Text(
                "\r\nInvalid choice, keeping current mode.\r\n".to_string(),
            )])?;
            current
        }
    };

    Ok(selected)
}
