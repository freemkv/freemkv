// The tracing output of a test's call into the ripper, captured on its own thread, so a
// test can check what an operation logged for the operator's diagnostics.
use std::sync::{Arc, Mutex};

pub(crate) fn traced<T>(f: impl FnOnce() -> T) -> (T, String) {
    use tracing_subscriber::layer::SubscriberExt as _;
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }
    let buf = Buf::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(buf.clone())
            .with_ansi(false),
    );
    // A second live dispatcher: tracing-core then asks every one for a callsite's interest,
    // not the default of whichever (subscriber-less) test thread hit it first.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    let out = tracing::subscriber::with_default(subscriber, f);
    let text =
        String::from_utf8_lossy(&buf.0.lock().unwrap_or_else(|p| p.into_inner())).into_owned();
    (out, text)
}
