//! The main window's content: the four pages the core switches between
//! (empty / titles / progress / result) above the log. Built once per locale;
//! `render` only assigns `View` fields to it.

use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use super::Shell;
use super::tree::TitleTree;
use crate::linux_glue::{self as glue, LogDelta, LogMemo};
use crate::ui::{Cmd, Effect, LogKind, LogLine, View};

use std::rc::Rc;

/// What was last painted — never model state, only "has this changed?".
#[derive(Default)]
pub(super) struct Memo {
    rows: String,
    formats: String,
    log: LogMemo,
}

pub(super) struct MainView {
    pub root: gtk::Box,
    stack: gtk::Stack,
    tree: TitleTree,
    out_entry: gtk::Entry,
    fmt_drop: gtk::DropDown,
    fmt_list: gtk::StringList,
    run_btn: gtk::Button,
    eject_btn: gtk::Button,
    detail: gtk::TextView,
    info_vals: Vec<gtk::Label>,
    saving_cur: gtk::Label,
    cap_cur: gtk::Label,
    bar_cur: gtk::ProgressBar,
    all_row: gtk::Box,
    saving_all: gtk::Label,
    cap_all: gtk::Label,
    bar_all: gtk::ProgressBar,
    result: adw::StatusPage,
    log_scroll: gtk::ScrolledWindow,
    log_view: gtk::TextView,
    log_end: gtk::TextMark,
    tags: [gtk::TextTag; 3],
}

fn g(key: &str) -> String {
    crate::strings::get(key)
}

fn frame(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Frame {
    let f = gtk::Frame::new(Some(title));
    f.set_child(Some(child));
    f
}

fn padded_box(orientation: gtk::Orientation, spacing: i32) -> gtk::Box {
    let b = gtk::Box::new(orientation, spacing);
    b.set_margin_top(8);
    b.set_margin_bottom(8);
    b.set_margin_start(8);
    b.set_margin_end(8);
    b
}

/// A caption whose digits do not jiggle as they tick (tabular figures).
fn caption() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(1.0);
    l.add_css_class("numeric");
    l
}

fn tag_index(kind: LogKind) -> usize {
    match kind {
        LogKind::Notice => 0,
        LogKind::Detail => 1,
        LogKind::Result => 2,
    }
}

/// Notice red / detail green like the macOS log, in the libadwaita palette's
/// light or dark shade so both stay readable.
fn paint_tags(tags: &[gtk::TextTag; 3], dark: bool) {
    let (red, green) = if dark {
        ("#ff7b63", "#8ff0a4")
    } else {
        ("#c01c28", "#1b8553")
    };
    tags[0].set_foreground(Some(red));
    tags[1].set_foreground(Some(green));
}

pub(super) fn build(shell: &Rc<Shell>) -> Rc<MainView> {
    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    stack.set_transition_type(gtk::StackTransitionType::None);

    // ── empty page: both ways in, side by side ──
    let open_disc = gtk::Button::with_label(&g("gui.btn.open_disc"));
    open_disc.add_css_class("pill");
    open_disc.add_css_class("suggested-action");
    let me = shell.clone();
    open_disc.connect_clicked(move |_| me.open_disc(true));
    let open_file = gtk::Button::with_label(&g("gui.btn.open_file"));
    open_file.add_css_class("pill");
    let me = shell.clone();
    open_file.connect_clicked(move |_| me.act(Cmd::Open));
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    buttons.append(&open_disc);
    buttons.append(&open_file);
    let empty = adw::StatusPage::builder()
        .icon_name("media-optical-symbolic")
        .title(g("gui.page.empty_title"))
        .description(glib::markup_escape_text(&g("gui.page.empty_subtitle")))
        .child(&buttons)
        .vexpand(true)
        .build();
    stack.add_named(&empty, Some(glue::page_name(crate::ui::Page::Empty)));

    // ── titles page: tree | output + info ──
    let me_sel = shell.clone();
    let me_tog = shell.clone();
    let tree = TitleTree::new(
        Rc::new(move |i| me_sel.select_row(i)),
        Rc::new(move |i| me_tog.toggle_row(i)),
    );

    let out_entry = gtk::Entry::new();
    out_entry.set_hexpand(true);
    let me = shell.clone();
    out_entry.connect_changed(move |e| {
        if me.painting.get() {
            return;
        }
        let t = e.text().to_string();
        if me.app.borrow().output_dir != t {
            me.app.borrow_mut().output_dir = t;
        }
    });
    let browse = gtk::Button::from_icon_name("folder-open-symbolic");
    browse.set_tooltip_text(Some(&g("gui.panel.output_msg")));
    let me = shell.clone();
    browse.connect_clicked(move |_| me.act(Cmd::SetOutput));

    let fmt_list = gtk::StringList::new(&[]);
    let fmt_drop = gtk::DropDown::new(Some(fmt_list.clone()), gtk::Expression::NONE);
    fmt_drop.set_hexpand(true);
    let me = shell.clone();
    let list = fmt_list.clone();
    fmt_drop.connect_selected_notify(move |d| {
        if me.painting.get() {
            return;
        }
        let Some(label) = list.string(d.selected()) else {
            return;
        };
        let (disc, mp4) = {
            let a = me.app.borrow();
            (!crate::ui::is_container(&a.source), a.mp4_possible())
        };
        if let Some(f) = crate::ui::format_from_label(&label, disc, mp4) {
            me.act(Cmd::SetFormat(f));
        }
    });
    let eject_btn = gtk::Button::with_label(&g("gui.menu.eject"));
    let me = shell.clone();
    eject_btn.connect_clicked(move |_| me.act(Cmd::Eject));
    let run_btn = gtk::Button::with_label(&g("gui.btn.run_now"));
    run_btn.add_css_class("suggested-action");
    let me = shell.clone();
    run_btn.connect_clicked(move |_| me.act(Cmd::Run));

    let out_grid = gtk::Grid::new();
    out_grid.set_row_spacing(8);
    out_grid.set_column_spacing(8);
    out_grid.set_margin_top(8);
    out_grid.set_margin_bottom(8);
    out_grid.set_margin_start(8);
    out_grid.set_margin_end(8);
    out_grid.attach(&out_entry, 0, 0, 2, 1);
    out_grid.attach(&browse, 2, 0, 1, 1);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.append(&eject_btn);
    actions.append(&run_btn);
    out_grid.attach(&fmt_drop, 0, 1, 1, 1);
    out_grid.attach(&actions, 1, 1, 2, 1);

    let detail = gtk::TextView::new();
    detail.set_editable(false);
    detail.set_cursor_visible(false);
    detail.set_wrap_mode(gtk::WrapMode::WordChar);
    detail.set_left_margin(8);
    detail.set_right_margin(8);
    detail.set_top_margin(6);
    detail.set_bottom_margin(6);
    let detail_scroll = gtk::ScrolledWindow::builder()
        .child(&detail)
        .vexpand(true)
        .build();

    let right = gtk::Box::new(gtk::Orientation::Vertical, 8);
    right.append(&frame(&g("gui.group.output"), &out_grid));
    right.append(&frame(&g("gui.group.info"), &detail_scroll));

    let titles = gtk::Paned::new(gtk::Orientation::Horizontal);
    titles.set_start_child(Some(&tree.widget));
    titles.set_end_child(Some(&right));
    titles.set_shrink_start_child(false);
    titles.set_shrink_end_child(false);
    // The other shells' 46.4 % tree share of the default window width.
    titles.set_position(540);
    titles.set_vexpand(true);
    stack.add_named(&titles, Some(glue::page_name(crate::ui::Page::Titles)));

    // ── progress page: Information, one or two bars, Cancel ──
    let info_grid = gtk::Grid::new();
    info_grid.set_column_spacing(12);
    info_grid.set_row_spacing(2);
    info_grid.set_margin_top(8);
    info_grid.set_margin_bottom(8);
    info_grid.set_margin_start(8);
    info_grid.set_margin_end(8);
    let mut info_vals = Vec::new();
    for (i, k) in crate::ui::InfoRows::labels().iter().enumerate() {
        let key = gtk::Label::new(Some(k));
        key.set_xalign(1.0);
        key.add_css_class("dim-label");
        let val = gtk::Label::new(None);
        val.set_xalign(0.0);
        val.set_hexpand(true);
        val.set_selectable(true);
        val.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        info_grid.attach(&key, 0, i as i32, 1, 1);
        info_grid.attach(&val, 1, i as i32, 1, 1);
        info_vals.push(val);
    }
    let bar_row = |saving: &gtk::Label, cap: &gtk::Label, bar: &gtk::ProgressBar| {
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        saving.set_xalign(0.0);
        saving.set_hexpand(true);
        head.append(saving);
        head.append(cap);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
        col.append(&head);
        col.append(bar);
        col
    };
    let (saving_cur, cap_cur, bar_cur) =
        (gtk::Label::new(None), caption(), gtk::ProgressBar::new());
    let (saving_all, cap_all, bar_all) =
        (gtk::Label::new(None), caption(), gtk::ProgressBar::new());
    let cur_row = bar_row(&saving_cur, &cap_cur, &bar_cur);
    let all_row = bar_row(&saving_all, &cap_all, &bar_all);
    let cancel = gtk::Button::with_label(&g("gui.btn.cancel"));
    cancel.add_css_class("destructive-action");
    cancel.set_halign(gtk::Align::End);
    let me = shell.clone();
    cancel.connect_clicked(move |_| me.act(Cmd::Cancel));
    let progress = gtk::Box::new(gtk::Orientation::Vertical, 12);
    progress.append(&frame(&g("gui.group.information"), &info_grid));
    progress.append(&cur_row);
    progress.append(&all_row);
    progress.append(&cancel);
    stack.add_named(&progress, Some(glue::page_name(crate::ui::Page::Progress)));

    // ── result page ──
    let reveal = gtk::Button::with_label(&super::show_folder_label());
    reveal.add_css_class("pill");
    let me = shell.clone();
    reveal.connect_clicked(move |_| {
        let d = me.app.borrow().output_dir.clone();
        me.perform(vec![Effect::Reveal(d)]);
    });
    let done = gtk::Button::with_label(&g("gui.btn.done"));
    done.add_css_class("pill");
    done.add_css_class("suggested-action");
    let me = shell.clone();
    done.connect_clicked(move |_| {
        let fx = me.app_mut(|a| a.dismiss_result());
        me.perform(fx);
    });
    let result_btns = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    result_btns.set_halign(gtk::Align::Center);
    result_btns.append(&reveal);
    result_btns.append(&done);
    let result = adw::StatusPage::builder().child(&result_btns).build();
    result.add_css_class("compact");
    stack.add_named(&result, Some(glue::page_name(crate::ui::Page::Result)));

    // ── log ──
    let log_view = gtk::TextView::new();
    log_view.set_editable(false);
    log_view.set_cursor_visible(false);
    log_view.set_monospace(true);
    log_view.set_wrap_mode(gtk::WrapMode::WordChar);
    log_view.set_left_margin(8);
    log_view.set_right_margin(8);
    log_view.set_top_margin(6);
    log_view.set_bottom_margin(6);
    let buf = log_view.buffer();
    let tags = [
        gtk::TextTag::new(Some("notice")),
        gtk::TextTag::new(Some("detail")),
        gtk::TextTag::new(Some("result")),
    ];
    for t in &tags {
        buf.tag_table().add(t);
    }
    let style = adw::StyleManager::default();
    paint_tags(&tags, style.is_dark());
    let dark_tags = tags.clone();
    style.connect_dark_notify(move |s| paint_tags(&dark_tags, s.is_dark()));
    let log_end = buf.create_mark(None, &buf.end_iter(), false);
    let log_scroll = gtk::ScrolledWindow::builder()
        .child(&log_view)
        .has_frame(true)
        .min_content_height(200)
        .build();

    let root = padded_box(gtk::Orientation::Vertical, 8);
    root.append(&stack);
    root.append(&log_scroll);

    Rc::new(MainView {
        root,
        stack,
        tree,
        out_entry,
        fmt_drop,
        fmt_list,
        run_btn,
        eject_btn,
        detail,
        info_vals,
        saving_cur,
        cap_cur,
        bar_cur,
        all_row,
        saving_all,
        cap_all,
        bar_all,
        result,
        log_scroll,
        log_view,
        log_end,
        tags,
    })
}

fn set_buffer_text(tv: &gtk::TextView, text: &str) {
    let buf = tv.buffer();
    let (s, e) = buf.bounds();
    if buf.text(&s, &e, false) != text {
        buf.set_text(text);
    }
}

impl MainView {
    /// Assign the view. Computes nothing the core already decided.
    pub(super) fn render(&self, v: &View, memo: &mut Memo) {
        self.stack.set_visible_child_name(glue::page_name(v.page));
        let log_fills = glue::log_fills(v.page) && !v.log_hidden;
        self.stack.set_vexpand(!log_fills);
        self.log_scroll.set_vexpand(log_fills);
        self.log_scroll.set_visible(!v.log_hidden);

        let sig = glue::rows_sig(&v.title_rows);
        if memo.rows != sig {
            self.tree.rebuild(&v.title_rows);
            memo.rows = sig;
        } else {
            self.tree.sync_checks(&v.title_rows);
        }
        set_buffer_text(&self.detail, &v.detail);

        if self.out_entry.text() != v.output_dir {
            self.out_entry.set_text(&v.output_dir);
        }
        self.run_btn.set_sensitive(v.can_run);
        self.eject_btn.set_visible(v.eject_visible);

        let labels = glue::flat_format_labels(&v.formats);
        let fsig = labels.join("\n");
        if memo.formats != fsig {
            let rows: Vec<&str> = labels.iter().map(String::as_str).collect();
            self.fmt_list.splice(0, self.fmt_list.n_items(), &rows);
            memo.formats = fsig;
        }
        let want = crate::ui::format_label(&v.format);
        let idx = labels.iter().position(|l| *l == want).unwrap_or(0) as u32;
        if self.fmt_drop.selected() != idx {
            self.fmt_drop.set_selected(idx);
        }

        if let Some(info) = &v.info {
            for (l, val) in self.info_vals.iter().zip(info.iter()) {
                l.set_text(val);
            }
        }
        self.saving_cur.set_text(&v.saving_current);
        self.cap_cur.set_text(&v.caption_current);
        self.bar_cur
            .set_fraction((v.bar_current / 100.0).clamp(0.0, 1.0));
        self.all_row.set_visible(v.show_overall_bar);
        self.saving_all.set_text(&v.saving_overall);
        self.cap_all.set_text(&v.caption_overall);
        self.bar_all
            .set_fraction((v.bar_overall / 100.0).clamp(0.0, 1.0));

        self.result.set_title(&v.result_heading);
        self.result
            .set_description(Some(&glib::markup_escape_text(&v.result_summary)));

        self.render_log(&v.log, memo);
    }

    fn append_log(&self, lines: &[LogLine]) {
        let buf = self.log_view.buffer();
        for l in lines {
            let mut end = buf.end_iter();
            buf.insert_with_tags(
                &mut end,
                &format!("{}\n", l.text),
                &[&self.tags[tag_index(l.kind)]],
            );
        }
    }

    // Append when the screen is still a prefix of the log, rewrite otherwise,
    // and keep the newest line in view either way.
    fn render_log(&self, log: &[LogLine], memo: &mut Memo) {
        match glue::log_delta(&memo.log, log) {
            LogDelta::Same => return,
            LogDelta::Append(from) => self.append_log(&log[from..]),
            LogDelta::Rewrite => {
                self.log_view.buffer().set_text("");
                self.append_log(log);
            }
        }
        memo.log = LogMemo::of(log);
        let tv = self.log_view.clone();
        let mark = self.log_end.clone();
        glib::idle_add_local_once(move || tv.scroll_to_mark(&mark, 0.0, false, 0.0, 1.0));
    }
}
