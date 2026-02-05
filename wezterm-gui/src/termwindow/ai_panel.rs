use crate::termwindow::box_model::{
    BorderColor, BoxDimension, DisplayType, Element, ElementColors, ElementContent,
};
use crate::termwindow::modal::Modal;
use crate::termwindow::TermWindow;
use crate::termwindow::TermWindowNotif;
use crate::termwindow::UIItemType;
use anyhow::Context;
use config::{ConfigHandle, Dimension};
use euclid::rect;
use std::cell::{Ref, RefCell};
use std::process::Command;
use wezterm_term::{KeyCode, KeyModifiers, MouseEvent};
use window::color::LinearRgba;
use window::WindowOps;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelMode {
    Planner,
    ClaudeLike,
}

pub struct AiPanel {
    element: RefCell<Option<Vec<crate::termwindow::box_model::ComputedElement>>>,
    input: RefCell<String>,
    history: RefCell<Vec<String>>,
    output: RefCell<Vec<String>>,
    status: RefCell<String>,
    pending_command: RefCell<Option<String>>,
    mode: RefCell<PanelMode>,
    config: ConfigHandle,
}

impl AiPanel {
    pub fn new(config: ConfigHandle) -> Self {
        Self {
            element: RefCell::new(None),
            input: RefCell::new(String::new()),
            history: RefCell::new(Vec::new()),
            output: RefCell::new(Vec::new()),
            status: RefCell::new("Ready".to_string()),
            pending_command: RefCell::new(None),
            mode: RefCell::new(PanelMode::Planner),
            config,
        }
    }

    fn append_output(&self, line: impl Into<String>) {
        self.output.borrow_mut().push(line.into());
    }

    fn set_status(&self, status: impl Into<String>) {
        *self.status.borrow_mut() = status.into();
    }

    fn toggle_mode(&self) {
        let next = match *self.mode.borrow() {
            PanelMode::Planner => PanelMode::ClaudeLike,
            PanelMode::ClaudeLike => PanelMode::Planner,
        };
        *self.mode.borrow_mut() = next;
    }

    fn handle_submit(&self, term_window: &mut TermWindow) -> anyhow::Result<()> {
        let mut input = self.input.borrow_mut();
        let line = input.trim().to_string();
        input.clear();

        if line.is_empty() {
            return Ok(());
        }

        if line == "/mode" {
            self.toggle_mode();
            return Ok(());
        }

        if line == "/exit" {
            term_window.cancel_modal();
            return Ok(());
        }

        if line == "/help" {
            let mode = match *self.mode.borrow() {
                PanelMode::Planner => "Planner",
                PanelMode::ClaudeLike => "Claude",
            };
            self.append_output(format!("--- AI Panel [{mode}] commands ---"));
            self.append_output("/help     - show this list".to_string());
            self.append_output("/exit     - close the AI panel".to_string());
            self.append_output("/mode     - switch between Planner and Claude-like".to_string());
            self.append_output("Ctrl+M    - same as /mode".to_string());
            self.append_output(String::new());
            self.append_output("Planner mode:".to_string());
            self.append_output("  /plan <task>  - get a plan + optional Mermaid diagram".to_string());
            self.append_output("  <prompt>      - get action or command suggestion (y/n to run)".to_string());
            self.append_output(String::new());
            self.append_output("Claude-like mode:".to_string());
            self.append_output("  <prompt>      - runs: ollama run <model> <prompt>".to_string());
            term_window.invalidate_modal();
            return Ok(());
        }

        let active_pane_id = term_window.get_active_pane_no_overlay().map(|p| p.pane_id());

        if let Some(pending) = self.pending_command.borrow_mut().take() {
            match line.as_str() {
                "y" | "Y" | "yes" => {
                    if let (Some(window), Some(pane_id)) =
                        (&term_window.window, active_pane_id)
                    {
                        self.append_output(format!("Running in terminal: {pending}"));
                        window.notify(TermWindowNotif::EmitOutputForPane {
                            pane_id,
                            text: format!("{}\r\n", pending),
                        });
                    } else {
                        self.append_output("No active terminal pane.".to_string());
                    }
                }
                _ => {
                    self.append_output("Command skipped.".to_string());
                }
            }
            term_window.invalidate_modal();
            return Ok(());
        }

        self.history.borrow_mut().push(line.clone());
        let mode = *self.mode.borrow();
        let config = self.config.clone();
        let output = self.output.clone();
        let status = self.status.clone();
        let pending = self.pending_command.clone();
        let window = term_window.window.clone().unwrap();
        let code_mode_auto_accept = config.code_mode_auto_accept;

        self.set_status("Running...");

        promise::spawn::spawn_into_new_thread(move || -> anyhow::Result<()> {
            let mut out_lines = vec![];
            match mode {
                PanelMode::ClaudeLike => {
                    let model = config
                        .ai_model
                        .clone()
                        .unwrap_or_else(|| "qwen2.5-coder:7b".to_string());
                    let _ = crate::ollama::ensure_ollama_ready(
                        &config.ai_ollama_base_url,
                        config.ai_model.as_deref(),
                        |_| {},
                    );
                    let result = run_ollama_cli(&model, &line)?;
                    out_lines.push(result);
                }
                PanelMode::Planner => {
                    if let Some(task) = line.strip_prefix("/plan ") {
                        let plan = crate::code_mode::request_plan(&config, task)?;
                        out_lines.push(format!("Plan: {}", plan.summary));
                        if !plan.steps.is_empty() {
                            out_lines.push("Steps:".to_string());
                            for (idx, step) in plan.steps.iter().enumerate() {
                                out_lines.push(format!("  {}. {}", idx + 1, step));
                            }
                        }
                        if let Some(mermaid) = plan.mermaid {
                            crate::mermaid_overlay::show_mermaid(&mermaid);
                            out_lines.push(mermaid);
                        }
                    } else {
                        let action = crate::code_mode::request_action(&config, &line)?;
                        if action.kind == "command" {
                            if code_mode_auto_accept {
                                out_lines.push(format!("Running in terminal: {}", action.command));
                                if let Some(pane_id) = active_pane_id {
                                    window.notify(TermWindowNotif::EmitOutputForPane {
                                        pane_id,
                                        text: format!("{}\r\n", action.command),
                                    });
                                }
                            } else {
                                pending.borrow_mut().replace(action.command.clone());
                                out_lines.push(format!(
                                    "Command suggested:\n{}\nApprove? (y/n)",
                                    action.command
                                ));
                            }
                        } else {
                            out_lines.push(action.content);
                        }
                    }
                }
            }

            let mut output = output.borrow_mut();
            for line in out_lines {
                output.push(line);
            }
            *status.borrow_mut() = "Ready".to_string();

            window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                term_window.invalidate_modal();
            })));

            Ok(())
        })
        .detach();

        Ok(())
    }
}

impl Modal for AiPanel {
    fn mouse_event(&self, _event: MouseEvent, _term_window: &mut TermWindow) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<bool> {
        match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE) => {
                term_window.cancel_modal();
                return Ok(true);
            }
            (KeyCode::Char('m'), KeyModifiers::CTRL) => {
                self.toggle_mode();
                term_window.invalidate_modal();
                return Ok(true);
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                self.input.borrow_mut().pop();
                term_window.invalidate_modal();
                return Ok(true);
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                self.handle_submit(term_window)?;
                term_window.invalidate_modal();
                return Ok(true);
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                self.input.borrow_mut().push(c);
                term_window.invalidate_modal();
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn computed_element(
        &self,
        term_window: &mut TermWindow,
    ) -> anyhow::Result<Ref<'_, [crate::termwindow::box_model::ComputedElement]>> {
        if self.element.borrow().is_none() {
            let font_style = term_window
                .config
                .command_palette_font
                .as_ref()
                .unwrap_or(&term_window.config.font);
            let font = term_window
                .fonts
                .resolve_font(font_style)
                .context("resolve ai panel font")?;
            let metrics = crate::utilsprites::RenderMetrics::with_font_metrics(&font.metrics());
            let (padding_left, _padding_top) = term_window.padding_left_top();
            let border = term_window.get_os_border();
            let tab_bar_height = if term_window.show_tab_bar && term_window.config.tab_bar_at_bottom {
                term_window.tab_bar_pixel_height().unwrap_or(0.)
            } else {
                0.
            };
            let panel_width = 520.;
            let panel_height = 280.;
            let x = padding_left + border.left.get() as f32;
            let y = term_window.dimensions.pixel_height as f32
                - panel_height
                - border.bottom.get() as f32
                - tab_bar_height;

            let mode = match *self.mode.borrow() {
                PanelMode::Planner => "Planner",
                PanelMode::ClaudeLike => "Claude",
            };

            let mut lines = Vec::new();
            lines.push(format!(
                "AI Panel [{mode}]  /help  |  Type here. Click terminal to run commands there."
            ));
            lines.push(format!("Status: {}", self.status.borrow()));
            lines.push(String::new());
            lines.push("History:".to_string());
            for item in self.history.borrow().iter().rev().take(4).rev() {
                lines.push(format!("- {item}"));
            }
            lines.push(String::new());
            lines.push("Output:".to_string());
            for item in self.output.borrow().iter().rev().take(6).rev() {
                lines.push(item.clone());
            }
            lines.push(String::new());
            lines.push(format!("> {}", self.input.borrow()));

            let panel_text_color = LinearRgba::with_components(0.90, 0.94, 1.0, 1.0);
            let mut elements = Vec::new();
            for line in lines {
                let text = if line.is_empty() {
                    " ".to_string()
                } else {
                    line
                };
                elements.push(
                    Element::new(&font, ElementContent::Text(text))
                        .display(DisplayType::Block)
                        .colors(ElementColors {
                            border: BorderColor::default(),
                            bg: LinearRgba::TRANSPARENT.into(),
                            text: panel_text_color.into(),
                        }),
                );
            }

            let element = Element::new(&font, ElementContent::Children(elements))
                .display(DisplayType::Block)
                .item_type(UIItemType::Modal)
                .colors(ElementColors {
                    border: BorderColor::new(LinearRgba::with_components(0.12, 0.17, 0.23, 1.0)),
                    bg: LinearRgba::with_components(0.04, 0.06, 0.10, 0.92).into(),
                    text: panel_text_color.into(),
                })
                .padding(BoxDimension {
                    left: Dimension::Cells(0.5),
                    right: Dimension::Cells(0.5),
                    top: Dimension::Cells(0.5),
                    bottom: Dimension::Cells(0.5),
                });

            let computed = term_window.compute_element(
                &crate::termwindow::box_model::LayoutContext {
                    height: config::DimensionContext {
                        dpi: term_window.dimensions.dpi as f32,
                        pixel_max: term_window.dimensions.pixel_height as f32,
                        pixel_cell: metrics.cell_size.height as f32,
                    },
                    width: config::DimensionContext {
                        dpi: term_window.dimensions.dpi as f32,
                        pixel_max: term_window.dimensions.pixel_width as f32,
                        pixel_cell: metrics.cell_size.width as f32,
                    },
                    bounds: rect(x, y, panel_width, panel_height),
                    metrics: &metrics,
                    gl_state: term_window.render_state.as_ref().unwrap(),
                    zindex: 100,
                },
                &element,
            )?;

            self.element.borrow_mut().replace(vec![computed]);
        }

        Ok(Ref::map(self.element.borrow(), |e| {
            e.as_ref().unwrap().as_slice()
        }))
    }

    fn reconfigure(&self, _term_window: &mut TermWindow) {
        self.element.borrow_mut().take();
    }
}

fn run_ollama_cli(model: &str, prompt: &str) -> anyhow::Result<String> {
    let output = Command::new("ollama")
        .args(["run", model, prompt])
        .output()
        .context("running ollama")?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(stdout)
    } else if !stderr.is_empty() {
        Ok(stderr)
    } else {
        Ok(stdout)
    }
}

