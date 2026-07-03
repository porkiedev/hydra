#![allow(unused)]

mod modem;

use anyhow::{anyhow, Result};
use std::cmp::PartialEq;
use std::default::Default;
use std::env::current_exe;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use eframe::Frame;
use egui::{Align, ComboBox, Layout, RichText, Ui, WidgetText};
use egui_dock::{DockArea, DockState, Style, TabPath};
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, EnumIter, IntoEnumIterator};
use tokio::runtime::{Handle, Runtime};
use tracing::{error, info, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use crate::modem::{Bandwidth, Modem};

fn main() -> Result<()> {
    // Filter logs from other crates
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(Level::ERROR)
        .with_target(module_path!(), Level::TRACE);
    // Initialize the logger
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    // Initialize the tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");
    let _enter = rt.enter();

    // Load the config
    let config = HydraConfig::load()?;

    // Run the window
    eframe::run_native(
        "Hydra TNC",
        eframe::NativeOptions {
            ..Default::default()
        },
        Box::new(|cc| {Ok(Box::new(Hydra {
            rt,
            tree: DockState::new(vec![Tab::ModemConnection]),
            config,
            state: HydraState::default()
        }))})
    )?;

    info!("Exiting...");
    Ok(())
}

/// The application wrapper
struct Hydra {
    rt: Runtime,
    tree: DockState<Tab>,
    config: HydraConfig,
    state: HydraState
}

impl eframe::App for Hydra {
    fn ui(&mut self, ui: &mut Ui, frame: &mut Frame) {

        // Top/menu bar
        egui::Panel::top("menu_bar").show(ui, |ui| {
            ui.with_layout(Layout::left_to_right(Align::LEFT), |ui| {

                // Shows the current station callsign
                ui.strong(&self.config.callsign);

                ui.separator();

                // Allows for toggling certain tabs
                ComboBox::from_id_salt("view_combobox")
                    .selected_text("View")
                    .show_ui(ui, |ui| {
                        for t in Tab::iter().filter(|t| t.can_be_added()) {
                            // Look for the tab in the current tree
                            let existing_tab = self.tree.find_tab_from(|t2| &t == t2);
                            let already_visible = existing_tab.is_some();
                            // Create a label for each tab type
                            if ui.selectable_label(already_visible, t.brief()).clicked() {
                                match existing_tab {
                                    // The tab already exists, remove it
                                    Some(tab_path) => {
                                        self.tree.remove_tab(tab_path);
                                    }
                                    // The tab doesn't exist, add it
                                    None => {
                                        self.tree.push_to_focused_leaf(t);
                                    }
                                }
                            };

                        }
                    });
            });
        });

        // Central panel/tabbed layout
        egui::CentralPanel::default().show(ui, |ui| {

            DockArea::new(&mut self.tree)
                .style(Style::from_egui(ui.style().as_ref()))
                .show_inside(ui, &mut TabViewer { config: &mut self.config, state: &mut self.state });

        });
    }
}

/// The persistent config for Hydra
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
struct HydraConfig {
    /// The ip address of the mercury instance
    mercury_host: IpAddr,
    /// The ARQ port of the mercury instance
    mercury_base_port: u16,
    /// The Data port of the mercury instance (usually ARQ/Base port + 1)
    mercury_data_port: u16,
    /// The bandwidth setting
    mercury_bandwidth: Bandwidth,
    /// The operator callsign
    callsign: String,
}
impl Default for HydraConfig {
    fn default() -> Self {
        Self {
            mercury_host: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            mercury_base_port: 8300,
            mercury_data_port: 8301,
            mercury_bandwidth: Bandwidth::BW500,
            callsign: String::new(),
        }
    }
}
impl HydraConfig {
    /// Save the config. This is generally only called when a new config is made because one doesn't already exist
    fn save(&self) -> Result<()> {
        info!("Saving config...");

        // Create the path. This is stored alongside the executable if possible
        let path = match current_exe()?.parent() {
            Some(path) => path.join("hydra-config.json"),
            None => PathBuf::from("hydra-config.json"),
        };
        // Create the file
        let mut file = File::create(path)?;
        // Write the serialized config to the file
        serde_json::to_writer_pretty(file, self)?;

        Ok(())
    }

    /// Load the config from the config file (or create a new one if it doesn't exist)
    fn load() -> Result<Self> {

        // Create the path
        let path = match current_exe()?.parent() {
            Some(path) => path.join("hydra-config.json"),
            None => PathBuf::from("hydra-config.json"),
        };

        // Open and read the file into a string, or create a new config if it doesn't already exist
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) => {
                return if e.kind() == ErrorKind::NotFound {
                    info!("The config file wasn't found; creating a new one for you");
                    let s = Self::default();
                    s.save()?;
                    Ok(s)
                } else {
                    Err(e.into())
                }
            }
        };
        let mut ser_self = String::new();
        file.read_to_string(&mut ser_self)?;
        // Deserialize the string
        let s: Self = serde_json::from_str(&ser_self)?;

        // Perform some sanity checks on the config
        let mut should_abort = false;
        if s.callsign.is_empty() {
            error!("No callsign was specified in the config");
            should_abort = true;
        }
        if should_abort {
            return Err(anyhow!("Config is not in a usable state. Please see the above error for details"))?
        }

        // Deserialize the file
        Ok(s)
    }
}

/// The runtime state for Hydra
#[derive(Debug)]
struct HydraState {
    /// A handle to the modem
    modem: Modem,
    /// Whether listening is enabled
    modem_listen: bool,
    /// Whether to accept incoming connections regardless of destination callsign
    modem_public: bool,
    /// The destination callsign textbox
    destination_callsign: String,
}
impl Default for HydraState {
    fn default() -> Self {
        Self {
            modem: Modem::default(),
            modem_listen: false,
            modem_public: false,
            destination_callsign: String::new(),
        }
    }
}

#[derive(EnumIter, PartialEq)]
enum Tab {
    /// Welcomes the user
    Welcome,
    /// For controlling and connecting to the mercury modem
    ModemConnection,
    /// Overall Packet monitor
    Monitor,
}
impl Tab {
    /// Whether the tab can/should be added by the user
    fn can_be_added(&self) -> bool {
        // Currently, every tab can be added except the welcome tab
        !matches!(self, Self::Welcome)
    }

    /// Brief name for the tab that can be shown in the view dropdown
    fn brief(&self) -> &'static str {
        match self {
            Tab::Welcome => "Welcome",
            Tab::ModemConnection => "Modem",
            Tab::Monitor => "Monitor",
        }
    }
}

struct TabViewer<'a> {
    config: &'a mut HydraConfig,
    state: &'a mut HydraState
}
impl egui_dock::TabViewer for TabViewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Self::Tab) -> WidgetText {
        tab.brief().into()
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Welcome => {
                ui.heading("Welcome to Hydra, a TNC for the Mercury modem");
            }
            Tab::ModemConnection => {

                // Determine if the modem is connected
                let mercury_connected = self.state.modem.is_mercury_connected();

                // Connect/Disconnect from mercury button
                if mercury_connected {
                    if ui.button("Disconnect").clicked() { self.state.modem.disconnect_mercury() };
                }
                else {
                    if ui.button("Connect").clicked() {
                        self.state.modem.connect_mercury(
                            format!("{}:{}", self.config.mercury_host, self.config.mercury_base_port),
                            self.config.callsign.clone(),
                            true,
                            self.config.mercury_bandwidth
                        );
                    };
                }

                // Only enable the various configuration widgets if we are connected to the modem
                ui.add_enabled_ui(mercury_connected, |ui| {

                    // Bandwidth selection
                    ComboBox::from_id_salt("bandwidth_combobox")
                        .selected_text("Bandwidth")
                        .show_ui(ui, |ui| {
                            for opt in Bandwidth::iter() {
                                if ui.selectable_label(opt == self.config.mercury_bandwidth, opt.to_string()).clicked() {
                                    // Update bandwidth
                                    self.config.mercury_bandwidth = opt;
                                    self.state.modem.set_bandwidth(opt);
                                    if let Err(e) = self.config.save() {
                                        error!("Failed to save config: {e}");
                                    }
                                };

                            }
                        });

                    // Listen checkbox
                    if ui.checkbox(&mut self.state.modem_listen, "Listen")
                        .on_hover_text("Enter the LISTENING state and accept incoming CALL frames addressed to your callsign")
                        .clicked() {
                        self.state.modem.set_listen(self.state.modem_listen);
                    }

                    // Public checkbox
                    if ui.checkbox(&mut self.state.modem_public, "Public")
                        .on_hover_text("Accept incoming CALL frames regardless of the destination callsign")
                        .clicked() {
                        self.state.modem.set_public(self.state.modem_public);
                    }

                    ui.separator();

                    ui.label(format!("Bytes remaining in buffer: {}", self.state.modem.get_tx_buffer_len()));

                    ui.separator();

                    ui.label("Destination callsign");
                    ui.text_edit_singleline(&mut self.state.destination_callsign);
                    let station_connected = self.state.modem.is_connected();
                    if station_connected && ui.button("Disconnect from station").clicked() {
                        self.state.modem.disconnect();
                    }
                    else if ui.button("Connect to station").clicked() {
                        self.state.modem.connect(&self.config.callsign, &self.state.destination_callsign);
                    }

                });

            }
            Tab::Monitor => {
                ui.label("Todo");
            }
        }
    }
}
