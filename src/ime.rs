//! 输入法（IME）组字状态的判断。

/// 这一帧输入法还在忙：要么有没提交完的候选串（preedit），要么刚好把一个词
/// 提交进了输入框。忙的时候方向键、回车、Esc 都是输入法在用（选候选词、
/// 确认、撤销），不能同时当成界面自己的「提交 / 关闭 / 移动选中项」。
pub(crate) fn composing(ui: &egui::Ui) -> bool {
    ui.input(|i| {
        i.events.iter().any(|event| match event {
            egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => !text.is_empty(),
            egui::Event::Ime(egui::ImeEvent::Commit(_)) => true,
            _ => false,
        })
    })
}

/// 鼠标按住的那几帧，把这一帧的输入法锚点撤掉。
///
/// egui 在拖选的每一帧都会对已聚焦的 TextEdit 调 `request_focus`，而
/// `request_focus` 会顺带置位「打断组字」；egui-winit 看到它就
/// `set_ime_allowed(false)` 紧跟 `set_ime_allowed(true)`，等于一帧一次把
/// 输入法上下文从窗口卸下再挂回来，输入法图标跟着一闪一闪。
///
/// 按住鼠标这段时间本来也不打字，锚点干脆不给：整段拖动只剩「卸下 / 挂回」
/// 各一次，松手那一帧锚点自己就回来了。
pub(crate) fn hide_ime_anchor_while_pointer_held(ctx: &egui::Context) {
    if !ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    ctx.output_mut(|o| o.ime = None);
}

/// 给浮层输入框要焦点，只在它确实没焦点时要。
///
/// `Response::request_focus` 会顺带打断输入法组字（egui 的
/// `Memory::request_focus` 文档里明写了这一点），每帧无条件调用等于拼音还没
/// 打完就被清掉，中文根本填不进去。
///
/// 之所以仍然要坚持要焦点：对话框刚弹出时得能直接打字，而且面板的键盘归属
/// 判定会把焦点抢回终端（见 `app.rs` 里给面板发焦点的那段）。
pub(crate) fn focus_input_box(resp: &egui::Response) {
    if !resp.has_focus() {
        resp.request_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composing_after(events: Vec<egui::Event>) -> bool {
        let ctx = egui::Context::default();
        let mut composing_now = false;
        let mut out = ctx.run_pass(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 300.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                ctx.root_ui(|ui| {
                    ui.label("probe");
                    composing_now = composing(ui);
                });
            },
        );
        // 没有渲染器消费字体图集的增量，epaint 会在 drop 时断言
        out.textures_delta.clear();
        composing_now
    }

    fn preedit(text: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Preedit {
            text: text.into(),
            active_range_chars: None,
        })
    }

    #[test]
    fn non_empty_preedit_counts_as_composing() {
        assert!(composing_after(vec![preedit("wenjian")]));
    }

    /// 空的 preedit 是「输入法撤了」，这时候回车该归界面。
    #[test]
    fn empty_preedit_is_not_composing() {
        assert!(!composing_after(vec![preedit("")]));
    }

    /// 用回车确认候选词时，Commit 和那一下回车会在同一帧里到齐。
    #[test]
    fn commit_counts_as_composing() {
        assert!(composing_after(vec![
            preedit("文件"),
            egui::Event::Ime(egui::ImeEvent::Commit("文件".into())),
        ]));
    }

    /// 平时打字（按键 + Text 事件）不该被当成组字，否则回车建不了文件。
    #[test]
    fn plain_typing_is_not_composing() {
        assert!(!composing_after(vec![
            egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::Text("a".into()),
        ]));
        assert!(!composing_after(vec![]));
    }
}
