use lazy_static::lazy_static;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Mutex;
use std::thread;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

lazy_static! {
    static ref MERMAID_SENDER: Mutex<Option<Sender<String>>> = Mutex::new(None);
}

pub fn show_mermaid(diagram: &str) {
    let mut sender_guard = MERMAID_SENDER.lock().unwrap();
    if sender_guard.is_none() {
        let (tx, rx) = mpsc::channel::<String>();
        *sender_guard = Some(tx.clone());
        spawn_mermaid_window(rx);
    }
    if let Some(sender) = sender_guard.as_ref() {
        let _ = sender.send(diagram.to_string());
    }
}

fn spawn_mermaid_window(rx: Receiver<String>) {
    thread::spawn(move || {
        let event_loop: EventLoop<()> = EventLoop::new();

        let window = WindowBuilder::new()
            .with_title("WezTerm Plan")
            .with_inner_size(LogicalSize::new(600.0, 420.0))
            .build(&event_loop)
            .expect("create mermaid window");

        if let Some(monitor) = window.current_monitor() {
            let size = monitor.size();
            let pos = LogicalPosition::new(
                (size.width.saturating_sub(620)) as f64,
                20.0,
            );
            let _ = window.set_outer_position(pos);
        }

        let html = r#"
<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8"/>
  <script src="https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.min.js"></script>
  <style>
    body { margin: 0; font-family: sans-serif; background: #111; color: #eee; }
    #container { padding: 12px; }
  </style>
</head>
<body>
  <div id="container">
    <div id="diagram">Waiting for plan...</div>
  </div>
  <script>
    mermaid.initialize({ startOnLoad: false, theme: "dark" });
    async function renderDiagram(diagram) {
      try {
        const id = "mermaid-diagram";
        const { svg } = await mermaid.render(id, diagram);
        document.getElementById("diagram").innerHTML = svg;
      } catch (e) {
        document.getElementById("diagram").innerText = "Mermaid error: " + e;
      }
    }
    window.renderDiagram = renderDiagram;
  </script>
</body>
</html>
"#;

        let webview = WebViewBuilder::new(&window)
            .with_html(html)
            .build()
            .expect("build mermaid webview");

        event_loop.run(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::MainEventsCleared => {
                    loop {
                        match rx.try_recv() {
                            Ok(diagram) => {
                                let script = format!(
                                    "window.renderDiagram({});",
                                    serde_json::to_string(&diagram)
                                        .unwrap_or_else(|_| "\"\"".to_string())
                                );
                                let _ = webview.evaluate_script(&script);
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => break,
                        }
                    }
                }
                Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        });
    });
}
