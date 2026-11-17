use super::*;

#[derive(Default)]
struct Record {
    lines: Mutex<Vec<String>>,
    cancel: bool,
}

impl Sink for Record {
    fn log(&self, _level: Level, msg: &str) {
        self.lines.lock().unwrap().push(format!("log {msg}"));
    }

    fn title_opened(&self, title: &libfreemkv::DiscTitle) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("title {}", title.duration_secs));
    }

    fn progress(&self, p: &Progress) {
        self.lines
            .lock()
            .unwrap()
            .push(format!("progress {}", p.bytes_done));
    }

    fn event(&self, e: &Event<'_>) {
        let line = match e {
            Event::Phase { name } => format!("phase {name}"),
            Event::TitleStart { idx, .. } => format!("start {idx}"),
            _ => "other".into(),
        };
        self.lines.lock().unwrap().push(line);
    }

    fn should_cancel(&self) -> bool {
        self.cancel
    }
}

#[test]
fn the_capture_forwards_all_but_the_engines_replace_and_notes_the_title() {
    let inner = Record {
        cancel: true,
        ..Record::default()
    };
    let capture = Capture {
        inner: &inner,
        title: Mutex::new((None, None)),
    };
    capture.log(Level::Warn, "w");
    let mut title = libfreemkv::DiscTitle::empty();
    title.duration_secs = 90.0;
    capture.title_opened(&title);
    capture.progress(&Progress {
        bytes_done: 5,
        ..Default::default()
    });
    capture.event(&Event::TitleStart {
        idx: 2,
        dest: "mkv:///s/A.mkv",
    });
    capture.event(&Event::Phase { name: "replace" });
    capture.event(&Event::Phase { name: "mux" });
    assert!(capture.should_cancel());
    assert_eq!(*capture.title.lock().unwrap(), (Some(2), Some(90.0)));
    assert_eq!(
        *inner.lines.lock().unwrap(),
        vec![
            "log w".to_string(),
            "title 90".into(),
            "progress 5".into(),
            "start 2".into(),
            "phase mux".into(),
        ]
    );
}

#[test]
fn a_kept_error_reads_as_its_cause() {
    let kept = Kept {
        path: PathBuf::from("/s/A.staged.mkv"),
        sidecar: PathBuf::from("/s/A.staged.json"),
        phase: "copy",
        cause: io::Error::other("the share went away"),
    };
    assert_eq!(kept.to_string(), "the share went away");
    assert!(std::error::Error::source(&kept).is_none());
    let e = io::Error::new(io::ErrorKind::TimedOut, kept);
    let found = kept_of(&e).expect("the kept file travels with the error");
    assert_eq!(found.phase, "copy");
    assert!(kept_of(&io::Error::other("plain")).is_none());
}
