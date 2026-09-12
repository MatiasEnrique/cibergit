#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("cibergit V1 supports macOS on Apple Silicon only");

use gpui::{prelude::*, *};
use gpui_base::input::{Editor, EditorState, InputEditorStyle};
use std::path::PathBuf;

actions!(cibergit, [Save, Quit]);

struct Workspace {
    editor: Entity<EditorState>,
    path: Option<PathBuf>,
    disk_base: Option<String>,
    message: String,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>, path: Option<PathBuf>) -> Self {
        let loaded = path.as_ref().map(std::fs::read_to_string);
        let (disk_base, text, message) = match loaded {
            Some(Ok(text)) => (Some(text.clone()), text, "⌘S to save".into()),
            Some(Err(error)) => (None, String::new(), format!("Cannot open file: {error}")),
            None => (
                None,
                "// Welcome to cibergit\n// Native editor foundation\n".into(),
                "Pass a text file path to edit and save it".into(),
            ),
        };
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx);
            state.set_editor_style(InputEditorStyle {
                foreground: rgb(0xe6e8ec).into(),
                muted_foreground: rgb(0xa0a7b3).into(),
                background: rgb(0x17191d).into(),
                editor_gutter_background: Some(rgb(0x17191d).into()),
                ..Default::default()
            });
            state.set_value(text, window, cx);
            state.focus(window, cx);
            state
        });
        #[cfg(feature = "ui-smoke")]
        if let Some(output) = std::env::var_os("CIBERGIT_SMOKE_DIR") {
            let output = PathBuf::from(output);
            let entity = cx.entity();
            window.on_next_frame(move |window, cx| {
                let image = window.render_to_image().expect("native Metal readback");
                image.save(output.join("native-editor.png")).expect("save native capture");
                entity.update(cx, |this, cx| {
                    this.editor.update(cx, |editor, cx| {
                        editor.replace_all("// Edited through GPUI Kit\n", window, cx);
                    });
                    this.save(&Save, window, cx);
                    assert_eq!(this.message, "Saved");
                    assert_eq!(std::fs::read_to_string(this.path.as_ref().unwrap()).unwrap(), "// Edited through GPUI Kit\n");
                });
                std::fs::write(output.join("native-smoke.txt"), "Native GPUI Metal render captured; GPUI Kit buffer edited; Save handler round-trip passed.\n").unwrap();
                cx.quit();
            });
        }
        Self {
            editor,
            path,
            disk_base,
            message,
        }
    }

    fn save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(path), Some(base)) = (&self.path, &self.disk_base) else {
            self.message = "Open an existing text file before saving".into();
            cx.notify();
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        self.message = match std::fs::read_to_string(path) {
            Ok(current) if current == *base => match std::fs::write(path, &text) {
                Ok(()) => {
                    self.disk_base = Some(text);
                    "Saved".into()
                }
                Err(error) => format!("Save failed: {error}"),
            },
            Ok(_) => {
                "File changed on disk; unsaved text preserved. Reconciliation is pending.".into()
            }
            Err(error) => format!("Cannot verify disk contents: {error}"),
        };
        cx.notify();
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x17191d))
            .text_color(rgb(0xe6e8ec))
            .on_action(cx.listener(Self::save))
            .child(
                div()
                    .px_5()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(0x30343b))
                    .child("cibergit")
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xa0a7b3))
                            .child("Local editor · Milestone 0"),
                    ),
            )
            .child(
                div().px_5().py_2().text_sm().child(
                    self.path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "Untitled".into()),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_5()
                    .font_family("Menlo")
                    .text_size(px(14.))
                    .child(Editor::new(&self.editor)),
            )
            .child(
                div()
                    .px_5()
                    .py_3()
                    .flex()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(0x30343b))
                    .child(self.message.clone())
                    .child(
                        div()
                            .id("save")
                            .cursor_pointer()
                            .px_3()
                            .rounded_md()
                            .bg(rgb(0x343a46))
                            .child("Save  ⌘S")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save(&Save, window, cx)),
                            ),
                    ),
            )
    }
}

fn main() {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    gpui_platform::application().run(move |cx| {
        gpui_base::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-s", Save, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("cibergit".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(760.)),
                    cx,
                ))),
                ..Default::default()
            },
            move |window, cx| cx.new(|cx| Workspace::new(window, cx, path)),
        )
        .expect("open native window");
        cx.activate(true);
    });
}
