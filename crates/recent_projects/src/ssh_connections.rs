use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use auto_update::AutoUpdater;
use editor::Editor;
use futures::channel::oneshot;
use gpui::{
    percentage, px, Animation, AnimationExt, AnyWindowHandle, AsyncAppContext, DismissEvent,
    EventEmitter, FocusableView, ParentElement as _, Render, SemanticVersion, SharedString, Task,
    Transformation, View,
};
use gpui::{AppContext, Model};
use release_channel::{AppVersion, ReleaseChannel};
use remote::{SshConnectionOptions, SshPlatform, SshRemoteClient};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsSources};
use ui::{
    div, h_flex, prelude::*, v_flex, ActiveTheme, Color, Icon, IconName, IconSize,
    InteractiveElement, IntoElement, Label, LabelCommon, Styled, ViewContext, VisualContext,
    WindowContext,
};
use workspace::{AppState, ModalView, Workspace};

#[derive(Deserialize)]
pub struct SshSettings {
    pub ssh_connections: Option<Vec<SshConnection>>,
}

impl SshSettings {
    pub fn ssh_connections(&self) -> impl Iterator<Item = SshConnection> {
        self.ssh_connections.clone().into_iter().flatten()
    }
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SshConnection {
    pub host: SharedString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub projects: Vec<SshProject>,
    /// Name to use for this server in UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<SharedString>,
}
impl From<SshConnection> for SshConnectionOptions {
    fn from(val: SshConnection) -> Self {
        SshConnectionOptions {
            host: val.host.into(),
            username: val.username,
            port: val.port,
            password: None,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SshProject {
    pub paths: Vec<String>,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RemoteSettingsContent {
    pub ssh_connections: Option<Vec<SshConnection>>,
}

impl Settings for SshSettings {
    const KEY: Option<&'static str> = None;

    type FileContent = RemoteSettingsContent;

    fn load(sources: SettingsSources<Self::FileContent>, _: &mut AppContext) -> Result<Self> {
        sources.json_merge()
    }
}

pub struct SshPrompt {
    connection_string: SharedString,
    status_message: Option<SharedString>,
    error_message: Option<SharedString>,
    prompt: Option<(SharedString, oneshot::Sender<Result<String>>)>,
    editor: View<Editor>,
}

pub struct SshConnectionModal {
    pub(crate) prompt: View<SshPrompt>,
    is_separate_window: bool,
}

impl SshPrompt {
    pub(crate) fn new(
        connection_options: &SshConnectionOptions,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let connection_string = connection_options.connection_string().into();
        Self {
            connection_string,
            status_message: None,
            error_message: None,
            prompt: None,
            editor: cx.new_view(Editor::single_line),
        }
    }

    pub fn set_prompt(
        &mut self,
        prompt: String,
        tx: oneshot::Sender<Result<String>>,
        cx: &mut ViewContext<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            if prompt.contains("yes/no") {
                editor.set_masked(false, cx);
            } else {
                editor.set_masked(true, cx);
            }
        });
        self.prompt = Some((prompt.into(), tx));
        self.status_message.take();
        cx.focus_view(&self.editor);
        cx.notify();
    }

    pub fn set_status(&mut self, status: Option<String>, cx: &mut ViewContext<Self>) {
        self.status_message = status.map(|s| s.into());
        cx.notify();
    }

    pub fn set_error(&mut self, error_message: String, cx: &mut ViewContext<Self>) {
        self.error_message = Some(error_message.into());
        cx.notify();
    }

    pub fn confirm(&mut self, cx: &mut ViewContext<Self>) {
        if let Some((_, tx)) = self.prompt.take() {
            self.editor.update(cx, |editor, cx| {
                tx.send(Ok(editor.text(cx))).ok();
                editor.clear(cx);
            });
        }
    }
}

impl Render for SshPrompt {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let cx = cx.window_context();
        let theme = cx.theme();
        v_flex()
            .key_context("PasswordPrompt")
            .size_full()
            .justify_center()
            .child(
                h_flex()
                    .p_2()
                    .justify_center()
                    .flex_wrap()
                    .child(if self.error_message.is_some() {
                        Icon::new(IconName::XCircle)
                            .size(IconSize::Medium)
                            .color(Color::Error)
                            .into_any_element()
                    } else {
                        Icon::new(IconName::ArrowCircle)
                            .size(IconSize::Medium)
                            .with_animation(
                                "arrow-circle",
                                Animation::new(Duration::from_secs(2)).repeat(),
                                |icon, delta| {
                                    icon.transform(Transformation::rotate(percentage(delta)))
                                },
                            )
                            .into_any_element()
                    })
                    .child(
                        div()
                            .ml_1()
                            .child(Label::new("SSH Connection").size(LabelSize::Small)),
                    )
                    .child(
                        div()
                            .text_ellipsis()
                            .overflow_x_hidden()
                            .when_some(self.error_message.as_ref(), |el, error| {
                                el.child(Label::new(format!("－{}", error)).size(LabelSize::Small))
                            })
                            .when(
                                self.error_message.is_none() && self.status_message.is_some(),
                                |el| {
                                    el.child(
                                        Label::new(format!(
                                            "－{}",
                                            self.status_message.clone().unwrap()
                                        ))
                                        .size(LabelSize::Small),
                                    )
                                },
                            ),
                    ),
            )
            .child(div().when_some(self.prompt.as_ref(), |el, prompt| {
                el.child(
                    h_flex()
                        .p_4()
                        .border_t_1()
                        .border_color(theme.colors().border_variant)
                        .font_buffer(cx)
                        .child(Label::new(prompt.0.clone()))
                        .child(self.editor.clone()),
                )
            }))
    }
}

impl SshConnectionModal {
    pub fn new(
        connection_options: &SshConnectionOptions,
        is_separate_window: bool,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        Self {
            prompt: cx.new_view(|cx| SshPrompt::new(connection_options, cx)),
            is_separate_window,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, cx: &mut ViewContext<Self>) {
        self.prompt.update(cx, |prompt, cx| prompt.confirm(cx))
    }

    fn dismiss(&mut self, _: &menu::Cancel, cx: &mut ViewContext<Self>) {
        cx.emit(DismissEvent);
        if self.is_separate_window {
            cx.remove_window();
        }
    }
}

pub(crate) struct SshConnectionHeader {
    pub(crate) connection_string: SharedString,
    pub(crate) nickname: Option<SharedString>,
}

impl RenderOnce for SshConnectionHeader {
    fn render(self, cx: &mut WindowContext) -> impl IntoElement {
        let theme = cx.theme();

        let mut header_color = theme.colors().text;
        header_color.fade_out(0.96);

        let (main_label, meta_label) = if let Some(nickname) = self.nickname {
            (nickname, Some(format!("({})", self.connection_string)))
        } else {
            (self.connection_string, None)
        };

        h_flex()
            .p_1()
            .rounded_t_md()
            .w_full()
            .gap_2()
            .justify_center()
            .border_b_1()
            .border_color(theme.colors().border_variant)
            .bg(header_color)
            .child(Icon::new(IconName::Server).size(IconSize::XSmall))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Label::new(main_label)
                            .size(ui::LabelSize::Small)
                            .single_line(),
                    )
                    .children(meta_label.map(|label| {
                        Label::new(label)
                            .size(ui::LabelSize::Small)
                            .single_line()
                            .color(Color::Muted)
                    })),
            )
    }
}

impl Render for SshConnectionModal {
    fn render(&mut self, cx: &mut ui::ViewContext<Self>) -> impl ui::IntoElement {
        let connection_string = self.prompt.read(cx).connection_string.clone();
        let theme = cx.theme();

        let body_color = theme.colors().editor_background;

        v_flex()
            .elevation_3(cx)
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::confirm))
            .w(px(500.))
            .border_1()
            .border_color(theme.colors().border)
            .child(
                SshConnectionHeader {
                    connection_string,
                    nickname: None,
                }
                .render(cx),
            )
            .child(
                h_flex()
                    .rounded_b_md()
                    .bg(body_color)
                    .w_full()
                    .child(self.prompt.clone()),
            )
    }
}

impl FocusableView for SshConnectionModal {
    fn focus_handle(&self, cx: &gpui::AppContext) -> gpui::FocusHandle {
        self.prompt.read(cx).editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for SshConnectionModal {}

impl ModalView for SshConnectionModal {}

#[derive(Clone)]
pub struct SshClientDelegate {
    window: AnyWindowHandle,
    ui: View<SshPrompt>,
    known_password: Option<String>,
}

impl remote::SshClientDelegate for SshClientDelegate {
    fn ask_password(
        &self,
        prompt: String,
        cx: &mut AsyncAppContext,
    ) -> oneshot::Receiver<Result<String>> {
        let (tx, rx) = oneshot::channel();
        let mut known_password = self.known_password.clone();
        if let Some(password) = known_password.take() {
            tx.send(Ok(password)).ok();
        } else {
            self.window
                .update(cx, |_, cx| {
                    self.ui.update(cx, |modal, cx| {
                        modal.set_prompt(prompt, tx, cx);
                    })
                })
                .ok();
        }
        rx
    }

    fn set_status(&self, status: Option<&str>, cx: &mut AsyncAppContext) {
        self.update_status(status, cx)
    }

    fn set_error(&self, error: String, cx: &mut AsyncAppContext) {
        self.update_error(error, cx)
    }

    fn get_server_binary(
        &self,
        platform: SshPlatform,
        cx: &mut AsyncAppContext,
    ) -> oneshot::Receiver<Result<(PathBuf, SemanticVersion)>> {
        let (tx, rx) = oneshot::channel();
        let this = self.clone();
        cx.spawn(|mut cx| async move {
            tx.send(this.get_server_binary_impl(platform, &mut cx).await)
                .ok();
        })
        .detach();
        rx
    }

    fn remote_server_binary_path(&self, cx: &mut AsyncAppContext) -> Result<PathBuf> {
        let release_channel = cx.update(|cx| ReleaseChannel::global(cx))?;
        Ok(format!(".local/zed-remote-server-{}", release_channel.dev_name()).into())
    }
}

impl SshClientDelegate {
    fn update_status(&self, status: Option<&str>, cx: &mut AsyncAppContext) {
        self.window
            .update(cx, |_, cx| {
                self.ui.update(cx, |modal, cx| {
                    modal.set_status(status.map(|s| s.to_string()), cx);
                })
            })
            .ok();
    }

    fn update_error(&self, error: String, cx: &mut AsyncAppContext) {
        self.window
            .update(cx, |_, cx| {
                self.ui.update(cx, |modal, cx| {
                    modal.set_error(error, cx);
                })
            })
            .ok();
    }

    async fn get_server_binary_impl(
        &self,
        platform: SshPlatform,
        cx: &mut AsyncAppContext,
    ) -> Result<(PathBuf, SemanticVersion)> {
        let (version, release_channel) = cx.update(|cx| {
            let global = AppVersion::global(cx);
            (global, ReleaseChannel::global(cx))
        })?;

        // In dev mode, build the remote server binary from source
        #[cfg(debug_assertions)]
        if release_channel == ReleaseChannel::Dev {
            let result = self.build_local(cx, platform, version).await?;
            // Fall through to a remote binary if we're not able to compile a local binary
            if let Some(result) = result {
                return Ok(result);
            }
        }

        self.update_status(Some("checking for latest version of remote server"), cx);
        let binary_path = AutoUpdater::get_latest_remote_server_release(
            platform.os,
            platform.arch,
            release_channel,
            cx,
        )
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to download remote server binary (os: {}, arch: {}): {}",
                platform.os,
                platform.arch,
                e
            )
        })?;

        Ok((binary_path, version))
    }

    #[cfg(debug_assertions)]
    async fn build_local(
        &self,
        cx: &mut AsyncAppContext,
        platform: SshPlatform,
        version: SemanticVersion,
    ) -> Result<Option<(PathBuf, SemanticVersion)>> {
        use smol::process::{Command, Stdio};

        async fn run_cmd(command: &mut Command) -> Result<()> {
            let output = command.stderr(Stdio::inherit()).output().await?;
            if !output.status.success() {
                Err(anyhow::anyhow!("failed to run command: {:?}", command))?;
            }
            Ok(())
        }

        if platform.arch == std::env::consts::ARCH && platform.os == std::env::consts::OS {
            self.update_status(Some("Building remote server binary from source"), cx);
            log::info!("building remote server binary from source");
            run_cmd(Command::new("cargo").args([
                "build",
                "--package",
                "remote_server",
                "--target-dir",
                "target/remote_server",
            ]))
            .await?;

            self.update_status(Some("Compressing binary"), cx);

            run_cmd(Command::new("gzip").args([
                "-9",
                "-f",
                "target/remote_server/debug/remote_server",
            ]))
            .await?;

            let path = std::env::current_dir()?.join("target/remote_server/debug/remote_server.gz");
            return Ok(Some((path, version)));
        } else if let Some(triple) = platform.triple() {
            smol::fs::create_dir_all("target/remote-server").await?;

            self.update_status(Some("Installing cross.rs for cross-compilation"), cx);
            log::info!("installing cross");
            run_cmd(Command::new("cargo").args([
                "install",
                "cross",
                "--git",
                "https://github.com/cross-rs/cross",
            ]))
            .await?;

            self.update_status(
                Some(&format!(
                    "Building remote server binary from source for {}",
                    &triple
                )),
                cx,
            );
            log::info!("building remote server binary from source for {}", &triple);
            run_cmd(
                Command::new("cross")
                    .args([
                        "build",
                        "--package",
                        "remote_server",
                        "--features",
                        "debug-embed",
                        "--target-dir",
                        "target/remote_server",
                        "--target",
                        &triple,
                    ])
                    .env(
                        "CROSS_CONTAINER_OPTS",
                        "--mount type=bind,src=./target,dst=/app/target",
                    ),
            )
            .await?;

            self.update_status(Some("Compressing binary"), cx);

            run_cmd(Command::new("gzip").args([
                "-9",
                "-f",
                &format!("target/remote_server/{}/debug/remote_server", triple),
            ]))
            .await?;

            let path = std::env::current_dir()?.join(format!(
                "target/remote_server/{}/debug/remote_server.gz",
                triple
            ));

            return Ok(Some((path, version)));
        } else {
            return Ok(None);
        }
    }
}

pub fn connect_over_ssh(
    unique_identifier: String,
    connection_options: SshConnectionOptions,
    ui: View<SshPrompt>,
    cx: &mut WindowContext,
) -> Task<Result<Model<SshRemoteClient>>> {
    let window = cx.window_handle();
    let known_password = connection_options.password.clone();

    remote::SshRemoteClient::new(
        unique_identifier,
        connection_options,
        Arc::new(SshClientDelegate {
            window,
            ui,
            known_password,
        }),
        cx,
    )
}

pub async fn open_ssh_project(
    connection_options: SshConnectionOptions,
    paths: Vec<PathBuf>,
    app_state: Arc<AppState>,
    open_options: workspace::OpenOptions,
    cx: &mut AsyncAppContext,
) -> Result<()> {
    let window = if let Some(window) = open_options.replace_window {
        window
    } else {
        let options = cx.update(|cx| (app_state.build_window_options)(None, cx))?;
        cx.open_window(options, |cx| {
            let project = project::Project::local(
                app_state.client.clone(),
                app_state.node_runtime.clone(),
                app_state.user_store.clone(),
                app_state.languages.clone(),
                app_state.fs.clone(),
                None,
                cx,
            );
            cx.new_view(|cx| Workspace::new(None, project, app_state.clone(), cx))
        })?
    };

    let delegate = window.update(cx, |workspace, cx| {
        cx.activate_window();
        workspace.toggle_modal(cx, |cx| {
            SshConnectionModal::new(&connection_options, true, cx)
        });
        let ui = workspace
            .active_modal::<SshConnectionModal>(cx)
            .unwrap()
            .read(cx)
            .prompt
            .clone();

        Arc::new(SshClientDelegate {
            window: cx.window_handle(),
            ui,
            known_password: connection_options.password.clone(),
        })
    })?;

    let did_open_ssh_project = cx
        .update(|cx| {
            workspace::open_ssh_project(
                window,
                connection_options,
                delegate.clone(),
                app_state,
                paths,
                cx,
            )
        })?
        .await;

    let did_open_ssh_project = match did_open_ssh_project {
        Ok(ok) => Ok(ok),
        Err(e) => {
            delegate.update_error(e.to_string(), cx);
            Err(e)
        }
    };

    did_open_ssh_project
}
