//! notts: one message, every channel you enabled.
//!
//! ```no_run
//! use notts::{Message, Notts, SendOptions, Severity};
//! let n = Notts::from_config(&notts::default_config_path()).expect("config");
//! let report = n.send(
//!     &Message::new("Disk almost full").body("/data at 97%").severity(Severity::Warn).app("judged"),
//!     &SendOptions::new(),
//! );
//! for r in &report.results {
//!     println!("{}: {} ({})", r.name, r.status.as_str(), r.detail);
//! }
//! std::process::exit(report.exit_code);
//! ```
//!
//! The public API is `Message`, `Severity`, `Notts`, `SendOptions`, `Report`, `ChannelResult`,
//! `Status`, `Disposition`, `Init`, `Config` and the `Clock` trait; everything else is internal
//! and may change in a minor version. No function here panics on bad input and nothing is global.

pub mod channels;
pub mod clock;
pub mod config;
pub mod digest;
pub mod engine;
pub mod history;
pub mod http;
pub mod lock;
pub mod message;
pub mod outbox;
pub mod policy;
pub mod secret;
pub mod state;
pub mod text;
pub mod watch;

pub use clock::{Clock, FakeClock, SystemClock};
pub use config::{Config, ConfigError};
pub use engine::{default_config_path, default_state_dir, ChannelResult, Disposition, Error, FlushReport, Init, Notts, Report, SendOptions, Status, CONTRACT, VERSION};
pub use message::{Message, Severity};
