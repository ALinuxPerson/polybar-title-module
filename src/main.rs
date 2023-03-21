use std::borrow::Cow;
use std::collections::HashMap;
use anyhow::Context;
use directories::ProjectDirs;
use figment::providers::{Format, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::process::ExitCode;
use std::{env, fmt, str};
use std::fmt::Formatter;
use std::str::FromStr;
use convert_case::{Case, Casing, Converter};
use handlebars::Handlebars;
use x11rb::connection::Connection;
use x11rb::properties::{WmClass, WmHints};
use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use serde_with::{serde_as, DisplayFromStr, DeserializeFromStr, SerializeDisplay};
use tracing::Level;

pub type NonNullWindow = NonZeroU32;

#[derive(Deserialize, Serialize, Debug)]
pub struct Config {
    pub display_name: Option<String>,

    #[serde(default = "template")]
    pub template: String,

    pub resolver: Resolver,
}

impl Config {
    pub fn read() -> anyhow::Result<Self> {
        let config_toml = ProjectDirs::from("", "ALinuxPerson", "polybar-title-module")
            .map(|pd| pd.config_dir().join("config.toml"));

        if config_toml.is_none() {
            tracing::warn!("could not get project directories");
        }

        let mut figment = Figment::new();

        if let Some(config_toml) = config_toml {
            figment = figment.join(Toml::file(config_toml))
        }

        figment = figment.join(Toml::file("polybar-title-module.toml"));

        figment.extract().context("failed to get config")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            display_name: None,
            template: template(),
            resolver: Resolver::default(),
        }
    }
}

#[derive(Eq, PartialEq, Hash, Copy, Clone, Debug)]
pub enum WindowIdentifierKind {
    Class,
    Name,
}

impl FromStr for WindowIdentifierKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "wm_class" | "wmc" | "wc" | "c" | "cls" | "wcls" | "class" => Ok(Self::Class),
            "wm_name" | "wmn" | "wn" | "n" | "name" => Ok(Self::Name),
            _ => anyhow::bail!("unknown window identifier kind"),
        }
    }
}

#[derive(DeserializeFromStr, SerializeDisplay, Eq, PartialEq, Hash, Debug)]
pub struct WindowIdentifier {
    pub kind: WindowIdentifierKind,
    pub value: String,
}

impl FromStr for WindowIdentifier {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (discriminant, value) = s.split_once('=').context("no '=' in window identifier")?;
        let kind = WindowIdentifierKind::from_str(discriminant).context("failed to resolve discriminant as a window identifier kind")?;

        Ok(Self {
            kind,
            value: value.to_string(),
        })
    }
}

impl fmt::Display for WindowIdentifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.kind {
            WindowIdentifierKind::Class => write!(f, "wm_class={}", self.value),
            WindowIdentifierKind::Name => write!(f, "wm_name={}", self.value),
        }
    }
}

#[serde_as]
#[derive(Deserialize, Serialize, Debug)]
pub struct Resolver {
    pub global_options: Option<Options>,
    pub desktop_name: Option<String>,
    pub filters: HashMap<WindowIdentifier, Filter>,
}

impl Resolver {
    pub fn resolve(&self, connection: &RustConnection, window: Window) -> anyhow::Result<String> {
        let Some(window) = NonNullWindow::new(window) else {
            tracing::debug!("window was 0, assuming it's desktop");
            return Ok(self.desktop_name.clone().unwrap_or_default())
        };

        tracing::debug!("retrieve WM_CLASS of window");
        let wm_class = WmClass::get(connection, window.get())
            .context("failed to make WmClass reply")?
            .reply()
            .context("WmClass response failed")?;
        let wm_class = str::from_utf8(wm_class.class()).context("WM_CLASS contains invalid utf-8")?;
        tracing::debug!(%wm_class, "WM_CLASS of window");

        tracing::debug!("retrieve WM_NAME of window");
        let wm_name = connection
            .get_property(false, window.get(), AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
            .context("failed to make GetProperty reply for retrieving WM_NAME")?
            .reply()
            .context("GetProperty response for retrieving WM_NAME failed")?
            .value;
        let wm_name = String::from_utf8(wm_name).context("WM_NAME contains invalid utf-8")?;
        tracing::debug!(%wm_name, "WM_NAME of window");

        tracing::debug!("find filter by WM_CLASS");
        let filter = self.filters.get(&WindowIdentifier {
            kind: WindowIdentifierKind::Class,
            value: wm_class.to_string(),
        })
            .or_else(|| {
                tracing::debug!("find filter by WM_NAME");

                self.filters.get(&WindowIdentifier {
                    kind: WindowIdentifierKind::Name,
                    value: wm_name.clone(),
                })
            })
            .map(Cow::Borrowed)
            .or_else(|| {
                tracing::debug!("falling back to global options");

                Some(Cow::Owned(Filter::Options(self.global_options?)))
            });
        let wm_class = if let Some(filter) = filter {
            tracing::debug!("resolve with filters");
            filter.resolve(wm_class)
        } else {
            tracing::debug!("no filters found, leaving WM_CLASS as is");
            Cow::Borrowed(wm_class)
        };

        Ok(wm_class.to_string())
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self {
            global_options: Some(Options {
                capitalize: Some(CapitalizeMode::default()),
            }),
            desktop_name: Some("Desktop".to_owned()),
            filters: HashMap::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(tag = "filter", content = "value", rename_all = "snake_case")]
pub enum Filter {
    Options(Options),
    NewName(String),
}

impl Filter {
    pub fn resolve<'wc>(&self, wm_class: &'wc str) -> Cow<'wc, str> {
        match self {
            Self::Options(options) => {
                tracing::debug!("resolving filter with options method");
                options.resolve(wm_class)
            },
            Self::NewName(name) => {
                tracing::debug!(%name, "resolving filter with new name method");
                Cow::Owned(name.clone())
            },
        }
    }
}

#[derive(Deserialize, Serialize, Copy, Clone, Debug)]
pub struct Options {
    pub capitalize: Option<CapitalizeMode>,
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|first_letter| first_letter.to_uppercase())
        .into_iter()
        .flatten()
        .chain(chars)
        .collect()
}

#[derive(Deserialize, Serialize, Copy, Clone, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CapitalizeMode {
    #[default]
    FirstLetter,

    AllWords,
}

impl CapitalizeMode {
    pub fn capitalize(&self, value: &str) -> String {
        match self {
            Self::FirstLetter => capitalize_first(value),
            Self::AllWords => value.to_case(Case::Title),
        }
    }
}

impl Options {
    pub fn resolve<'v>(&self, value: &'v str) -> Cow<'v, str> {
        let mut new_value = Cow::Borrowed(value);

        if let Some(capitalize) = &self.capitalize {
            tracing::debug!("capitalize value");
            new_value = Cow::Owned(capitalize.capitalize(&new_value))
        }

        new_value
    }
}

fn template() -> String {
    "{{ name }}".to_owned()
}

fn real_main() -> anyhow::Result<()> {
    if env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init()
    } else {
        tracing_subscriber::fmt()
            .with_max_level(Level::ERROR)
            .init()
    }

    tracing::debug!("parsing config");
    let config = Config::read().unwrap_or_else(|e| {
        tracing::warn!("could not parse config: {e:#}");
        Config::default()
    });

    tracing::debug!("create template registry and register template from config");
    let mut handlebars = Handlebars::new();
    handlebars.register_template_string("template", &config.template)
        .context("failed to register template string")?;

    tracing::info!("establishing a connection to the X server");
    let (connection, screen_num) = x11rb::connect(config.display_name.as_deref())
        .context("failed to establish a connection to the X server")?;

    tracing::debug!("get primary screen");
    let screen = &connection.setup().roots[screen_num];

    let events = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);

    tracing::info!("setting up events");
    connection
        .change_window_attributes(screen.root, &events)
        .context("failed to make ChangeWindowAttributes reply")?
        .check()
        .context("ChangeWindowAttributes response failed")?;


    loop {
        let event = connection
            .wait_for_event()
            .context("could not wait for event")?;

        if let Event::PropertyNotify(event) = event {
            tracing::debug!("got property notify event");
            let atom = connection
                .get_atom_name(event.atom)
                .context("failed to make GetAtomName reply")?
                .reply()
                .context("GetAtomName response failed")?;
            let atom_name =
                String::from_utf8(atom.name).context("atom name contains invalid utf-8")?;

            if atom_name == "_NET_ACTIVE_WINDOW" {
                tracing::debug!("atom name is _NET_ACTIVE_WINDOW, making reply to X server for properties");
                let property = connection
                    .get_property(false, event.window, event.atom, 33u32, 0, 4)
                    .context("failed to make GetProperty reply")?
                    .reply()
                    .context("GetProperty response failed")?;
                let value = property
                    .value32()
                    .context("failed to get u32 value from atom")?
                    .next()
                    .context("missing u32 value from atom")?;
                tracing::debug!(%value, "u32 property value");

                let window = NonNullWindow::new(value);

                tracing::debug!("resolving window name");
                let resolved_name = config.resolver
                    .resolve(&connection, window.map(|w| w.get()).unwrap_or_default())
                    .context("failed to resolve name of window")?;
                let mut data = HashMap::with_capacity(1);
                data.insert("name", resolved_name);

                tracing::debug!("rendering resolved name");
                let rendered_name = handlebars.render("template", &data).context("failed to render template")?;
                println!("{rendered_name}")
            } else {
                tracing::debug!(%atom_name, "other atom name was received")
            }
        } else {
            tracing::debug!(?event, "received other event")
        }
    }
}

fn main() -> ExitCode {
    if let Err(error) = real_main() {
        tracing::error!("{error:#}");
        println!("PolyBar title module crashed!");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
