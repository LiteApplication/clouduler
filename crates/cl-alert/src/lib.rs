//! Alerts: the catalog of events, the settings model, and delivery to
//! webhooks, Discord and email. The engine that decides what to send lives in
//! `cl-api` (it needs the cluster state); this crate only knows how to say it.

pub mod catalog;
pub mod deliver;
pub mod message;
pub mod settings;

pub use catalog::{Category, EventDef, EVENTS};
pub use deliver::{deliver, DeliveryError, DestinationSecrets};
pub use message::{AlertMessage, Body, Link, Notification, Phase};
pub use settings::{
    AlertSettings, Destination, DestinationKind, DiscordNotify, DiscordNotifyLevels,
    DiscordOptions, DiscordStyle, EventOverride, Route, Severity, SmtpSecurity, SmtpSettings,
    Thresholds,
};
