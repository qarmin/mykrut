use std::backtrace::Backtrace;
use std::sync::Once;

use tracing::error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Log targets that belong to us. Everything else is a third-party crate.
const OWN_CRATES: &[&str] = &["mykrut", "mykrut_core"];

pub fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = build_filter();

        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_thread_names(true)
            .with_line_number(true)
            .with_file(false)
            .compact();

        tracing_subscriber::registry().with(filter).with(fmt_layer).init();
    });
}

/// Third-party crates (e.g. `nusb`, which spams DEBUG while probing USB devices)
/// are floored at `warn`; only our own crates are verbose. `RUST_LOG` still works:
/// a bare level like `RUST_LOG=trace` sets our crates' verbosity, while explicit
/// per-target directives (`RUST_LOG=nusb=debug`) are honored as written.
fn build_filter() -> EnvFilter {
    let mut own_level = String::from("debug");
    let mut explicit = Vec::new();

    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        for part in rust_log.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if part.contains('=') {
                explicit.push(part.to_string());
            } else {
                own_level = part.to_string();
            }
        }
    }

    // `warn` global floor keeps foreign crates quiet unless asked for explicitly.
    let mut filter = EnvFilter::new("warn");
    for krate in OWN_CRATES {
        filter = filter.add_directive(
            format!("{krate}={own_level}")
                .parse()
                .expect("own-crate directive is well formed"),
        );
    }
    for directive in explicit {
        match directive.parse() {
            Ok(parsed) => filter = filter.add_directive(parsed),
            Err(err) => eprintln!("ignoring invalid RUST_LOG directive {directive:?}: {err}"),
        }
    }
    filter
}

pub struct PanicGuard;

pub fn install_panic_hook() -> PanicGuard {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = Backtrace::force_capture();
        let location = info
            .location()
            .map_or_else(|| "<unknown>".into(), |l| format!("{}:{}", l.file(), l.line()));
        let payload = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<no payload>");
        error!(
            location = %location,
            payload = %payload,
            backtrace = %bt,
            "PANIC"
        );
        default_hook(info);
    }));
    PanicGuard
}

#[macro_export]
macro_rules! op_span {
    ($name:expr $(, $field:tt = $value:expr)* $(,)?) => {
        ::tracing::info_span!($name $(, $field = $value)*)
    };
}

#[macro_export]
macro_rules! time_block {
    ($lvl:ident, $what:literal, $body:block) => {{
        let __start = ::std::time::Instant::now();
        let __out = { $body };
        ::tracing::$lvl!(elapsed_ms = __start.elapsed().as_millis() as u64, $what);
        __out
    }};
}
