//! The logger and the screen together (M10.1): the log file keeps every line exactly as before, and the screen shows the summary:
//! warnings and errors as such (an error with what to do), events in their own words, and every line only in verbose mode.

use std::io::Write;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use tenero_app::config::Raw;
use tenero_app::log::{Level, Logger};
use tenero_app::ui::{ColorChoice, Event, Screen, Theme, Verbosity};

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

fn screen(v: Verbosity, buf: &Buf) -> Arc<Screen> {
    // a plain screen (no terminal) whose clock stands still
    let clock = Arc::new(AtomicU64::new(1_700_000_000));
    Arc::new(Screen::new(
        Box::new(buf.clone()),
        false,
        Theme { color: false },
        v,
        60,
        Box::new(move || clock.load(std::sync::atomic::Ordering::SeqCst)),
    ))
}

fn tmp(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "tenero-logscreen-{}-{name}.log",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn logger(level: Level, file: &std::path::Path, sc: &Arc<Screen>) -> Logger {
    Logger::new(level, Some(file), false)
        .unwrap()
        .with_screen(Arc::clone(sc))
}

#[test]
fn an_info_line_goes_to_the_file_and_not_to_the_screen() {
    let (buf, f) = (Buf::default(), tmp("info"));
    let l = logger(Level::Info, &f, &screen(Verbosity::Normal, &buf));
    l.info("peer 3 connected to 127.0.0.4:18331");
    assert_eq!(buf.take(), "");
    let file = std::fs::read_to_string(&f).unwrap();
    assert!(
        file.ends_with("INFO  peer 3 connected to 127.0.0.4:18331\n"),
        "{file}"
    );
    let _ = std::fs::remove_file(&f);
}

#[test]
fn verbose_shows_the_line_of_the_log_as_it_is_in_the_file() {
    let (buf, f) = (Buf::default(), tmp("verbose"));
    let l = logger(Level::Info, &f, &screen(Verbosity::Verbose, &buf));
    l.info("peer 3 connected to 127.0.0.4:18331");
    l.debug("not logged at this level, so not shown");
    let shown = buf.take();
    let file = std::fs::read_to_string(&f).unwrap();
    assert_eq!(shown, file, "the screen shows what the file holds");
    assert_eq!(shown.lines().count(), 1);
    let _ = std::fs::remove_file(&f);
}

#[test]
fn warnings_and_errors_reach_the_screen_in_words_and_the_file_as_before() {
    let (buf, f) = (Buf::default(), tmp("warn"));
    let l = logger(Level::Info, &f, &screen(Verbosity::Normal, &buf));
    l.warn("could not save the side-branch pool: disk full");
    l.error("cannot listen: Address already in use (os error 98)");
    l.error("something nobody has seen");
    let shown = buf.take();
    assert_eq!(
        shown,
        "\
22:13:20  warning: could not save the side-branch pool: disk full
22:13:20  error: cannot listen: Address already in use (os error 98)
22:13:20    what to do: another program (perhaps another node) is using that port: stop it, or give this node a different `listen` or `control` address
22:13:20  error: something nobody has seen
"
    );
    let file = std::fs::read_to_string(&f).unwrap();
    assert!(file.contains("WARN  could not save the side-branch pool: disk full\n"));
    assert!(file.contains("ERROR cannot listen: Address already in use (os error 98)\n"));
    let _ = std::fs::remove_file(&f);
}

#[test]
fn an_event_shows_on_the_screen_in_its_own_words_and_in_the_file_as_the_line_it_always_was() {
    let (buf, f) = (Buf::default(), tmp("event"));
    let l = logger(Level::Info, &f, &screen(Verbosity::Normal, &buf));
    l.log_event(
        Level::Info,
        "stopped at height 7, tip 0063fcac",
        Event::Stopped {
            height: 7,
            tip: "0063fcac".into(),
        },
    );
    assert_eq!(buf.take(), "22:13:20  stopped at height 7 (0063fcac)\n");
    let file = std::fs::read_to_string(&f).unwrap();
    assert!(
        file.ends_with("INFO  stopped at height 7, tip 0063fcac\n"),
        "{file}"
    );
    let _ = std::fs::remove_file(&f);
}

#[test]
fn the_screen_shows_its_events_whatever_the_log_level_is_and_the_file_keeps_to_its_level() {
    let (buf, f) = (Buf::default(), tmp("level"));
    let l = logger(Level::Error, &f, &screen(Verbosity::Normal, &buf));
    l.log_event(
        Level::Info,
        "block 5 is in the chain",
        Event::Synced { height: 5 },
    );
    l.info("an info line: not logged at level error, not shown");
    l.warn("a warning: not logged at level error, so not shown either");
    assert_eq!(
        buf.take(),
        "22:13:20  synced: the chain is up to date at height 5\n"
    );
    assert_eq!(std::fs::read_to_string(&f).unwrap_or_default(), "");
    let _ = std::fs::remove_file(&f);
}

#[test]
fn quiet_shows_warnings_and_errors_only() {
    let (buf, f) = (Buf::default(), tmp("quiet"));
    let l = logger(Level::Info, &f, &screen(Verbosity::Quiet, &buf));
    l.info("detail");
    l.log_event(Level::Info, "up to date", Event::Synced { height: 5 });
    assert_eq!(buf.take(), "");
    l.warn("w");
    l.error("e");
    let shown = buf.take();
    assert_eq!(shown.lines().count(), 2, "{shown}");
    assert!(shown.contains("warning: w") && shown.contains("error: e"));
    // ... and the file has all of it
    assert_eq!(std::fs::read_to_string(&f).unwrap().lines().count(), 4);
    let _ = std::fs::remove_file(&f);
}

#[test]
fn a_newline_in_a_message_cannot_forge_a_line_on_the_screen_either() {
    let (buf, f) = (Buf::default(), tmp("forge"));
    let l = logger(Level::Info, &f, &screen(Verbosity::Normal, &buf));
    l.warn("a peer said\n22:13:20  error: the chain is gone");
    let shown = buf.take();
    assert_eq!(shown.lines().count(), 1, "{shown:?}");
    assert!(shown.starts_with("22:13:20  warning: a peer said 22:13:20  error: the chain is gone"));
    let _ = std::fs::remove_file(&f);
}

#[test]
fn a_logger_with_no_screen_writes_only_its_file() {
    let f = tmp("noscreen");
    let l = Logger::new(Level::Info, Some(&f), false).unwrap();
    assert!(l.screen().is_none());
    l.info("a line");
    l.log_event(Level::Info, "an event", Event::ShuttingDown);
    assert_eq!(std::fs::read_to_string(&f).unwrap().lines().count(), 2);
    let _ = std::fs::remove_file(&f);
}

// ---- the settings --------------------------------------------------------------------------------------------------------------------------

fn cfg(extra: &[&str]) -> Result<tenero_app::config::Config, String> {
    let args: Vec<String> = ["--data", "x", "--network", "test"]
        .iter()
        .chain(extra)
        .map(|s| s.to_string())
        .collect();
    Raw::default()
        .with_args(&args)
        .and_then(|r| r.into_config())
        .map_err(|e| e.to_string())
}

#[test]
fn quiet_verbose_and_colour_default_to_normal_output_in_auto_colour() {
    let c = cfg(&[]).unwrap();
    assert_eq!(
        (c.quiet, c.verbose, c.color),
        (false, false, ColorChoice::Auto)
    );
}

#[test]
fn quiet_and_verbose_can_stand_alone_or_take_yes_and_no() {
    assert!(cfg(&["--quiet"]).unwrap().quiet);
    assert!(cfg(&["--verbose"]).unwrap().verbose);
    // a bare flag before another option does not swallow it
    let c = cfg(&["--quiet", "--status_every", "5"]).unwrap();
    assert!(c.quiet);
    assert_eq!(c.status_every, 5);
    assert!(cfg(&["--verbose", "yes"]).unwrap().verbose);
    assert!(!cfg(&["--verbose", "no"]).unwrap().verbose);
    // a value that is neither is refused
    assert!(cfg(&["--quiet", "maybe"])
        .unwrap_err()
        .contains("`maybe` is not yes or no"));
    // given twice
    assert!(cfg(&["--quiet", "--quiet"])
        .unwrap_err()
        .contains("given twice"));
}

#[test]
fn quiet_and_verbose_together_are_refused_and_so_is_an_unknown_colour() {
    let e = cfg(&["--quiet", "--verbose"]).unwrap_err();
    assert!(e.contains("cannot be combined with `verbose`"), "{e}");
    assert_eq!(
        cfg(&["--color", "always"]).unwrap().color,
        ColorChoice::Always
    );
    assert_eq!(
        cfg(&["--color", "never"]).unwrap().color,
        ColorChoice::Never
    );
    assert_eq!(cfg(&["--color", "auto"]).unwrap().color, ColorChoice::Auto);
    let e = cfg(&["--color", "purple"]).unwrap_err();
    assert!(e.contains("`purple` is not auto, always or never"), "{e}");
}

#[test]
fn the_settings_file_may_say_the_same() {
    let text = "data = x\nnetwork = test\nquiet = yes\ncolor = never\n";
    let c = Raw::from_file_text(text).unwrap().into_config().unwrap();
    assert!(c.quiet && !c.verbose);
    assert_eq!(c.color, ColorChoice::Never);
}
