//! A clipboard button for resolved commands and file paths.
//!
//! Dioxus desktop has no Rust clipboard API without an extra crate, so the copy
//! goes through `document::eval` — the one place the GUI touches the webview's
//! browser API. `navigator.clipboard` needs a secure context, which the desktop
//! custom protocol may not provide, so the script falls back to a hidden
//! textarea plus `document.execCommand('copy')` inside the click gesture.
//!
//! Copy is never the only way out: every caller also renders the text in a
//! selectable box, so a refused clipboard is a manual Ctrl+C, not a dead end.

use dioxus::prelude::*;

/// Copy `text`, then report success back: `copied`, or `failed` after both
/// clipboard paths fail. A second message resets the label.
const COPY_SCRIPT: &str = r#"
    async function pitagamesCopy(text) {
        try {
            await navigator.clipboard.writeText(text);
            return true;
        } catch (error) {}
        const area = document.createElement('textarea');
        area.value = text;
        area.setAttribute('readonly', '');
        area.style.position = 'fixed';
        area.style.top = '-1000px';
        document.body.appendChild(area);
        area.select();
        let ok = false;
        try { ok = document.execCommand('copy'); } catch (error) { ok = false; }
        document.body.removeChild(area);
        return ok;
    }
    const text = await dioxus.recv();
    const ok = await pitagamesCopy(text);
    dioxus.send(ok ? 'copied' : 'failed');
    setTimeout(() => dioxus.send('reset'), 1600);
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyState {
    Idle,
    Copied,
    Failed,
}

#[derive(Props, Clone, PartialEq)]
pub struct CopyButtonProps {
    /// The text to copy. An empty string disables the button.
    pub text: String,
    /// The idle label; the button shows `Copied` / `Copy failed` transiently.
    #[props(default = "Copy".to_string())]
    pub label: String,
}

/// A small button that copies `text` to the system clipboard.
#[allow(non_snake_case)]
pub fn CopyButton(props: CopyButtonProps) -> Element {
    let text = props.text.clone();
    let label = props.label.clone();
    let disabled = text.is_empty();
    let mut state = use_signal(|| CopyState::Idle);

    let caption: &str = match state() {
        CopyState::Copied => "Copied",
        CopyState::Failed => "Copy failed",
        CopyState::Idle => label.as_str(),
    };

    rsx! {
        button {
            disabled,
            onclick: move |_| {
                let text = text.clone();
                spawn(async move {
                    let mut eval = document::eval(COPY_SCRIPT);
                    if eval.send(text).is_err() {
                        state.set(CopyState::Failed);
                        return;
                    }
                    while let Ok(message) = eval.recv::<String>().await {
                        match message.as_str() {
                            "copied" => state.set(CopyState::Copied),
                            "failed" => state.set(CopyState::Failed),
                            "reset" => state.set(CopyState::Idle),
                            _ => {}
                        }
                    }
                });
            },
            style: copy_style(disabled),
            "{caption}"
        }
    }
}

fn copy_style(disabled: bool) -> &'static str {
    if disabled {
        "padding:5px 12px;border-radius:7px;border:1px solid #26262f;background:#1a1a22;color:#5a5a68;font-size:11px;cursor:default;"
    } else {
        "padding:5px 12px;border-radius:7px;border:1px solid #3a3a4c;background:#2e3a56;color:#ffffff;font-size:11px;cursor:pointer;"
    }
}
