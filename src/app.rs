use crate::{
    config,
    config::{window_definition::WindowName, AnchorPoint},
    eww_state,
    script_var_handler::*,
    value::{AttrValue, Coords, NumWithUnit, PrimitiveValue, VarName},
    widgets,
};
use anyhow::*;
use debug_stub_derive::*;
use gtk4::{gdk, GtkWindowExt, StyleContextExt, WidgetExt};
use itertools::Itertools;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

/// Response that the app may send as a response to a event.
/// This is used in `DaemonCommand`s that contain a response sender.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, derive_more::Display)]
pub enum DaemonResponse {
    Success(String),
    Failure(String),
}

impl DaemonResponse {
    pub fn is_success(&self) -> bool {
        match self {
            DaemonResponse::Success(_) => true,
            _ => false,
        }
    }

    pub fn is_failure(&self) -> bool {
        !self.is_success()
    }
}

pub type DaemonResponseSender = tokio::sync::mpsc::UnboundedSender<DaemonResponse>;
pub type DaemonResponseReceiver = tokio::sync::mpsc::UnboundedReceiver<DaemonResponse>;

#[derive(Debug)]
pub enum DaemonCommand {
    NoOp,
    UpdateVars(Vec<(VarName, PrimitiveValue)>),
    ReloadConfig(config::EwwConfig),
    ReloadCss(String),
    OpenMany {
        windows: Vec<WindowName>,
        sender: DaemonResponseSender,
    },
    OpenWindow {
        window_name: WindowName,
        pos: Option<Coords>,
        size: Option<Coords>,
        anchor: Option<AnchorPoint>,
        sender: DaemonResponseSender,
    },
    CloseWindow {
        window_name: WindowName,
        sender: DaemonResponseSender,
    },
    KillServer,
    CloseAll,
    PrintState(DaemonResponseSender),
    PrintDebug(DaemonResponseSender),
    PrintWindows(DaemonResponseSender),
}

#[derive(Debug, Clone, PartialEq)]
pub struct EwwWindow {
    pub name: WindowName,
    pub definition: config::EwwWindowDefinition,
    pub gtk_window: gtk4::Window,
}

impl EwwWindow {
    pub fn close(self) {
        self.gtk_window.close();
    }
}

#[derive(DebugStub)]
pub struct App {
    pub eww_state: eww_state::EwwState,
    pub eww_config: config::EwwConfig,
    pub windows: HashMap<WindowName, EwwWindow>,
    pub css_provider: gtk4::CssProvider,
    pub app_evt_send: UnboundedSender<DaemonCommand>,
    #[debug_stub = "ScriptVarHandler(...)"]
    pub script_var_handler: ScriptVarHandlerHandle,
}

impl App {
    /// Handle a DaemonCommand event.
    pub fn handle_command(&mut self, event: DaemonCommand) {
        log::debug!("Handling event: {:?}", &event);
        let result: Result<_> = try {
            match event {
                DaemonCommand::NoOp => {}
                DaemonCommand::UpdateVars(mappings) => {
                    for (var_name, new_value) in mappings {
                        self.update_state(var_name, new_value)?;
                    }
                }
                DaemonCommand::ReloadConfig(config) => {
                    self.reload_all_windows(config)?;
                }
                DaemonCommand::ReloadCss(css) => {
                    self.load_css(&css);
                }
                DaemonCommand::KillServer => {
                    log::info!("Received kill command, stopping server!");
                    self.stop_application();
                    let _ = crate::application_lifecycle::send_exit();
                }
                DaemonCommand::CloseAll => {
                    log::info!("Received close command, closing all windows");
                    for (window_name, _window) in self.windows.clone() {
                        self.close_window(&window_name)?;
                    }
                }
                DaemonCommand::OpenMany { windows, sender } => {
                    let result = windows
                        .iter()
                        .map(|w| self.open_window(w, None, None, None))
                        .collect::<Result<()>>();
                    respond_with_error(sender, result)?;
                }
                DaemonCommand::OpenWindow {
                    window_name,
                    pos,
                    size,
                    anchor,
                    sender,
                } => {
                    let result = self.open_window(&window_name, pos, size, anchor);
                    respond_with_error(sender, result)?;
                }
                DaemonCommand::CloseWindow { window_name, sender } => {
                    let result = self.close_window(&window_name);
                    respond_with_error(sender, result)?;
                }
                DaemonCommand::PrintState(sender) => {
                    let output = self
                        .eww_state
                        .get_variables()
                        .iter()
                        .map(|(key, value)| format!("{}: {}", key, value))
                        .join("\n");
                    sender
                        .send(DaemonResponse::Success(output))
                        .context("Failed to send response from main thread")?
                }
                DaemonCommand::PrintWindows(sender) => {
                    let output = self
                        .eww_config
                        .get_windows()
                        .keys()
                        .map(|window_name| {
                            let is_open = self.windows.contains_key(window_name);
                            format!("{}{}", if is_open { "*" } else { "" }, window_name)
                        })
                        .join("\n");
                    sender
                        .send(DaemonResponse::Success(output))
                        .context("Failed to send response from main thread")?
                }
                DaemonCommand::PrintDebug(sender) => {
                    let output = format!("state: {:#?}\n\nconfig: {:#?}", &self.eww_state, &self.eww_config);
                    sender
                        .send(DaemonResponse::Success(output))
                        .context("Failed to send response from main thread")?
                }
            }
        };

        crate::print_result_err!("while handling event", &result);
    }

    fn stop_application(&mut self) {
        self.script_var_handler.stop_all();
        self.windows.drain().for_each(|(_, w)| w.close());
        crate::server::glib_stop_main();
    }

    fn update_state(&mut self, fieldname: VarName, value: PrimitiveValue) -> Result<()> {
        self.eww_state.update_variable(fieldname, value)
    }

    fn close_window(&mut self, window_name: &WindowName) -> Result<()> {
        for unused_var in self.variables_only_used_in(&window_name) {
            log::info!("stopping for {}", &unused_var);
            self.script_var_handler.stop_for_variable(unused_var.clone());
        }

        let window = self
            .windows
            .remove(window_name)
            .context(format!("No window with name '{}' is running.", window_name))?;

        window.close();
        self.eww_state.clear_window_state(window_name);

        Ok(())
    }

    fn open_window(
        &mut self,
        window_name: &WindowName,
        pos: Option<Coords>,
        size: Option<Coords>,
        anchor: Option<config::AnchorPoint>,
    ) -> Result<()> {
        // remove and close existing window with the same name
        let _ = self.close_window(window_name);

        log::info!("Opening window {}", window_name);

        let mut window_def = self.eww_config.get_window(window_name)?.clone();
        window_def.geometry = window_def.geometry.override_if_given(anchor, pos, size);

        let root_widget = widgets::widget_use_to_gtk_widget(
            &self.eww_config.get_widgets(),
            &mut self.eww_state,
            window_name,
            &maplit::hashmap! { "window_name".into() => AttrValue::from_primitive(window_name.to_string()) },
            &window_def.widget,
        )?;
        root_widget.get_style_context().add_class(&window_name.to_string());

        let monitor_geometry = get_monitor_geometry(window_def.screen_number.unwrap_or_else(get_default_monitor_index));
        let eww_window = initialize_window(monitor_geometry, root_widget, window_def)?;

        self.windows.insert(window_name.clone(), eww_window);

        // initialize script var handlers for variables that where not used before opening this window.
        // TODO somehow make this less shit
        for newly_used_var in self
            .variables_only_used_in(&window_name)
            .filter_map(|var| self.eww_config.get_script_var(&var))
        {
            self.script_var_handler.add(newly_used_var.clone());
        }

        Ok(())
    }

    pub fn reload_all_windows(&mut self, config: config::EwwConfig) -> Result<()> {
        log::info!("Reloading windows");
        // refresh script-var poll stuff
        self.script_var_handler.stop_all();

        self.eww_config = config;
        self.eww_state.clear_all_window_states();

        let windows = self.windows.clone();
        for (window_name, window) in windows {
            window.close();
            self.open_window(&window_name, None, None, None)?;
        }
        Ok(())
    }

    pub fn load_css(&mut self, css: &str) {
        // XXX This may error but gtk doesn't tell us,...
        self.css_provider.load_from_data(css.as_bytes());
    }

    /// Get all variable names that are currently referenced in any of the open windows.
    pub fn get_currently_used_variables(&self) -> impl Iterator<Item = &VarName> {
        self.windows
            .keys()
            .flat_map(move |window_name| self.eww_state.vars_referenced_in(window_name))
    }

    /// Get all variables mapped to a list of windows they are being used in.
    pub fn currently_used_variables<'a>(&'a self) -> HashMap<&'a VarName, Vec<&'a WindowName>> {
        let mut vars: HashMap<&'a VarName, Vec<_>> = HashMap::new();
        for window_name in self.windows.keys() {
            for var in self.eww_state.vars_referenced_in(window_name) {
                vars.entry(var)
                    .and_modify(|l| l.push(window_name))
                    .or_insert_with(|| vec![window_name]);
            }
        }
        vars
    }

    /// Get all variables that are only used in the given window.
    pub fn variables_only_used_in<'a>(&'a self, window: &'a WindowName) -> impl Iterator<Item = &'a VarName> {
        self.currently_used_variables()
            .into_iter()
            .filter(move |(_, wins)| wins.len() == 1 && wins.contains(&window))
            .map(|(var, _)| var)
    }
}

fn initialize_window(
    monitor_geometry: gdk::Rectangle,
    root_widget: gtk4::Widget,
    mut window_def: config::EwwWindowDefinition,
) -> Result<EwwWindow> {
    let actual_window_rect = window_def.geometry.get_window_rectangle(monitor_geometry);

    let window = gtk4::Window::new();
    window.set_focusable(window_def.focusable);

    window.set_title(Some(&format!("Eww - {}", window_def.name)));
    let wm_class_name = format!("eww-{}", window_def.name);
    // window.set_wmclass(&wm_class_name, &wm_class_name);
    if !window_def.focusable {
        // window.set_type_hint(gdk::WindowTypeHint::Dock);
    }
    // window.set_position(gtk4::WindowPosition::Center);
    window.set_default_size(actual_window_rect.width, actual_window_rect.height);
    window.set_size_request(actual_window_rect.width, actual_window_rect.height);
    window.set_decorated(false);
    window.set_resizable(false);

    window.set_child(Some(&root_widget));

    // Handle the fact that the gtk window will have a different size than specified,
    // as it is sized according to how much space it's contents require.
    // This is necessary to handle different anchors correctly in case the size was wrong.
    // XXX this won't work
    let (gtk_window_width, gtk_window_height) = window.get_default_size();
    window_def.geometry.size = Coords {
        x: NumWithUnit::Pixels(gtk_window_width),
        y: NumWithUnit::Pixels(gtk_window_height),
    };
    let actual_window_rect = window_def.geometry.get_window_rectangle(monitor_geometry);
    root_widget.show();
    window.set_visible(true);

    window.show();

    // let gdk_window = window.get_window().context("couldn't get gdk window from gtk window")?;
    // gdk_window.set_override_redirect(!window_def.focusable);
    // gdk_window.move_(actual_window_rect.x, actual_window_rect.y);

    // if window_def.stacking == WindowStacking::Foreground {
    // gdk_window.raise();
    // window.set_keep_above(true);
    //} else {
    // gdk_window.lower();
    // window.set_keep_below(true);
    //}

    Ok(EwwWindow {
        name: window_def.name.clone(),
        definition: window_def,
        gtk_window: window,
    })
}

/// get the index of the default monitor
fn get_default_monitor_index() -> i32 {
    // XXX This won't work
    0
}

/// Get the monitor geometry of a given monitor number
fn get_monitor_geometry(n: i32) -> gdk::Rectangle {
    // gdk::Display::get_default()
    //.expect("could not get default display")
    //.get_monitors().unwrap().cast
    //.get_monitor_geometry(n)

    // XXX
    gdk::Rectangle {
        x: 0,
        y: 0,
        width: 500,
        height: 500,
    }
}

/// In case of an Err, send the error message to a sender.
fn respond_with_error<T>(sender: DaemonResponseSender, result: Result<T>) -> Result<()> {
    match result {
        Ok(_) => sender.send(DaemonResponse::Success(String::new())),
        Err(e) => sender.send(DaemonResponse::Failure(format!("{:?}", e))),
    }
    .context("Failed to send response from main thread")
}
