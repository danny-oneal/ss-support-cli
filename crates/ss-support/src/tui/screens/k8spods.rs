//! The pods of one namespace, read through kubectl.

use std::cell::Cell;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ss_core::ops::s3::age;
use ss_core::svc::kubectl::{KubectlPort, Pod, parse_pods};
use unicode_width::UnicodeWidthStr;

use crate::tui::components::errorpanel::render_error;
use crate::tui::components::prompt::Prompt;
use crate::tui::screen::{Cmd, Ctx, Loaded, Msg, Screen};
use crate::tui::screens::logstail::LogsTail;
use crate::tui::text::clean;
use crate::tui::theme;

/// The column headings, in the order the cells are drawn.
const HEADINGS: [&str; 5] = ["NAME", "READY", "STATUS", "RESTARTS", "AGE"];

/// The pod list.
pub struct K8sPods {
    kubectl: Option<Arc<dyn KubectlPort>>,
    /// The namespace listed; empty for the context's default.
    namespace: String,
    pods: Vec<Pod>,
    selected: usize,
    /// The pod to select again when the listing in flight arrives.
    reselect: Option<String>,
    /// The namespace of the listing in flight.
    pending: Option<String>,
    /// The number of the last listing asked for; other results are stale.
    request: u64,
    /// Shown instead of a listing when there is none to show.
    error: Option<String>,
    /// Whether any listing has arrived yet.
    loaded: bool,
    /// Why the namespace listed, which has no pods, could not be confirmed
    /// to exist.
    unconfirmed: Option<String>,
    /// The namespace prompt, while it is open.
    asking: Option<Prompt>,
    now: fn() -> DateTime<Utc>,
    /// How many rows were last drawn; a page moves by this.
    page: Cell<usize>,
}

impl K8sPods {
    /// A pod list that starts on the context's default namespace.
    pub fn new(ctx: &Ctx) -> Self {
        let kubectl = ctx.deps.kubectl.clone();
        let error = kubectl
            .is_none()
            .then(|| "Kubernetes is not available in this session".to_owned());
        K8sPods {
            kubectl,
            namespace: String::new(),
            pods: Vec::new(),
            selected: 0,
            reselect: None,
            pending: None,
            request: 0,
            error,
            loaded: false,
            unconfirmed: None,
            asking: None,
            now: ctx.now,
            page: Cell::new(1),
        }
    }

    /// Asks for the pods of `namespace`. kubectl lists a namespace that does
    /// not exist as empty, so a named namespace without pods is looked up
    /// before it is shown as empty.
    fn load(&mut self, namespace: String) -> Vec<Cmd> {
        let Some(kubectl) = self.kubectl.clone() else {
            return Vec::new();
        };
        self.request += 1;
        self.pending = Some(namespace.clone());
        vec![Cmd::Task(
            self.request,
            Box::pin(async move {
                let listed = kubectl
                    .list_json(&namespace, "pods")
                    .await
                    .and_then(|raw| parse_pods(&raw));
                let pods = match listed {
                    Ok(pods) => pods,
                    Err(e) => return Loaded::Failed(format!("{:#}", anyhow::Error::new(e))),
                };
                if !pods.is_empty() || namespace.is_empty() {
                    return Loaded::Pods(pods);
                }
                match kubectl.get_json("", "namespace", &namespace).await {
                    Ok(_) => Loaded::Pods(pods),
                    Err(e) if e.is_not_found() => {
                        Loaded::Failed(format!("namespace {namespace:?} does not exist"))
                    }
                    Err(e) => Loaded::NoPodsUnconfirmed(format!("{:#}", anyhow::Error::new(e))),
                }
            }),
        )]
    }

    /// Shows the listing of `namespace` that has arrived.
    fn show(&mut self, namespace: String, pods: Vec<Pod>, unconfirmed: Option<String>) {
        let reselect = self.reselect.take();
        self.loaded = true;
        self.error = None;
        self.unconfirmed = unconfirmed;
        self.namespace = namespace;
        self.selected = reselect
            .and_then(|wanted| pods.iter().position(|pod| pod.name == wanted))
            .unwrap_or(0);
        self.pods = pods;
    }

    fn arrived(&mut self, loaded: Loaded) -> Vec<Cmd> {
        let Some(namespace) = self.pending.take() else {
            return Vec::new();
        };
        match loaded {
            Loaded::Pods(pods) => {
                self.show(namespace, pods, None);
                Vec::new()
            }
            Loaded::NoPodsUnconfirmed(reason) => {
                self.show(namespace, Vec::new(), Some(reason));
                Vec::new()
            }
            // With a listing on screen the failure is a status line and the
            // listing stays; without one it takes the screen.
            Loaded::Failed(e) if self.loaded => {
                self.reselect = None;
                vec![Cmd::Status(format!("Kubernetes: {e}"))]
            }
            Loaded::Failed(e) => {
                self.reselect = None;
                self.error = Some(e);
                Vec::new()
            }
            Loaded::Suggestions(_) | Loaded::Buckets(_) | Loaded::Page(_) | Loaded::Versions(_) => {
                Vec::new()
            }
        }
    }

    fn reload(&mut self) -> Vec<Cmd> {
        self.reselect = self.pods.get(self.selected).map(|pod| pod.name.clone());
        self.load(self.namespace.clone())
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Vec<Cmd> {
        if let Some(prompt) = &mut self.asking {
            match key.code {
                KeyCode::Esc => self.asking = None,
                // An empty answer is an answer here: the default namespace.
                KeyCode::Enter => {
                    let namespace = prompt.value().trim().to_owned();
                    self.asking = None;
                    return self.load(namespace);
                }
                _ => {
                    prompt.handle(key);
                }
            }
            return Vec::new();
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Vec::new();
        }
        let last = self.pods.len().saturating_sub(1);
        let page = self.page.get().max(1);
        match key.code {
            KeyCode::Esc => return vec![Cmd::Pop],
            KeyCode::Enter => return self.open(ctx),
            KeyCode::Char('r') => return self.reload(),
            KeyCode::Char('n') => self.asking = Some(Prompt::new("namespace")),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => self.selected = (self.selected + page).min(last),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            _ => {}
        }
        Vec::new()
    }

    /// Opens the log of the selected pod.
    fn open(&self, ctx: &Ctx) -> Vec<Cmd> {
        match self.pods.get(self.selected) {
            Some(pod) => vec![Cmd::Push(Box::new(LogsTail::new(
                &self.namespace,
                &pod.name,
                ctx,
            )))],
            None => Vec::new(),
        }
    }

    /// The cells of every row, headings first.
    fn rows(&self) -> Vec<[String; 5]> {
        let now = (self.now)();
        let mut rows = vec![HEADINGS.map(str::to_owned)];
        rows.extend(self.pods.iter().map(|pod| {
            [
                clean(&pod.name),
                clean(&pod.ready),
                clean(&pod.status),
                pod.restarts.to_string(),
                age(pod.created, now),
            ]
        }));
        rows
    }

    fn render_list(&self, frame: &mut Frame, area: Rect) {
        if let Some(error) = &self.error {
            render_error(frame, area, error);
            return;
        }
        if self.pods.is_empty() {
            let lines = match (&self.unconfirmed, self.loaded) {
                (Some(reason), _) => vec![
                    "no pods found".to_owned(),
                    clean(&format!("the namespace could not be confirmed: {reason}")),
                ],
                (None, true) => vec!["no pods in this namespace".to_owned()],
                (None, false) => vec!["loading…".to_owned()],
            };
            let lines: Vec<Line> = lines
                .into_iter()
                .map(|line| Line::styled(line, theme::dim()))
                .collect();
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            return;
        }
        let rows = self.rows();
        let mut widths = [0usize; 5];
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.width());
            }
        }
        let line = |row: &[String; 5]| {
            row.iter()
                .zip(widths)
                .map(|(cell, width)| {
                    format!("{cell}{}", " ".repeat(width.saturating_sub(cell.width())))
                })
                .collect::<Vec<_>>()
                .join("  ")
        };
        let [heading, list] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        if let Some(headings) = rows.first() {
            frame.render_widget(
                Line::styled(format!("  {}", line(headings)), theme::dim()),
                heading,
            );
        }
        let visible = usize::from(list.height);
        self.page.set(visible);
        let top = (self.selected + 1).saturating_sub(visible);
        let lines: Vec<Line> = rows
            .iter()
            .skip(1)
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(index, row)| {
                if index == self.selected {
                    Line::styled(format!("> {}", line(row)), theme::selected())
                } else {
                    Line::from(format!("  {}", line(row)))
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), list);
    }
}

impl Screen for K8sPods {
    fn title(&self) -> String {
        let place = match self.namespace.as_str() {
            "" => "default namespace",
            namespace => namespace,
        };
        let loading = if self.pending.is_some() {
            " · loading…"
        } else {
            ""
        };
        clean(&format!("Pods — {place}{loading}"))
    }

    fn hints(&self) -> String {
        if self.asking.is_some() {
            "enter list · empty for the default namespace · esc cancel"
        } else {
            "↑↓ move · enter logs · n namespace · r reload · esc back"
        }
        .to_owned()
    }

    fn init(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        self.load(String::new())
    }

    fn update(&mut self, msg: Msg, ctx: &Ctx) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) => self.on_key(key, ctx),
            Msg::Paste(text) => {
                // Text for the namespace being typed, and nothing otherwise.
                if let Some(prompt) = &mut self.asking {
                    prompt.paste(&text);
                }
                Vec::new()
            }
            Msg::Loaded(request, loaded) if request == self.request => self.arrived(loaded),
            Msg::Loaded(..)
            | Msg::Resize
            | Msg::Progress(_)
            | Msg::Warning(_)
            | Msg::Confirm(_)
            | Msg::RunDone(_)
            | Msg::Lines(..)
            | Msg::StreamEnded(..) => Vec::new(),
        }
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        let [list, prompt] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(self.asking.is_some())),
        ])
        .areas(area);
        self.render_list(frame, list);
        if let Some(asking) = &self.asking {
            asking.render(frame, prompt);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ss_core::svc::SvcError;
    use ss_core::svc::kubectl::{FakeKubectl, KubectlPort, Pod};
    use tokio::sync::mpsc;

    use super::*;
    use crate::tui::screen::Loaded;
    use crate::tui::shell::Shell;
    use crate::tui::testing::{ch, ctx, key, press, render_screen, render_shell, settle, typed};

    /// One pod: name, phase, each container's readiness, restarts, and the
    /// reason it is waiting (empty when it is running), and creation time.
    type PodRow<'a> = (&'a str, &'a str, &'a [(bool, u32, &'a str)], &'a str);

    /// A pod list as `kubectl get pods -o json` prints it.
    fn pods_json(pods: &[PodRow<'_>]) -> String {
        let items: Vec<String> = pods
            .iter()
            .map(|(name, phase, containers, created)| {
                let statuses: Vec<String> = containers
                    .iter()
                    .map(|(ready, restarts, waiting)| {
                        let state = if waiting.is_empty() {
                            r#"{"running":{}}"#.to_owned()
                        } else {
                            format!(r#"{{"waiting":{{"reason":{waiting:?}}}}}"#)
                        };
                        format!(
                            r#"{{"ready":{ready},"restartCount":{restarts},"state":{state}}}"#
                        )
                    })
                    .collect();
                format!(
                    r#"{{"metadata":{{"name":{name:?},"creationTimestamp":{created:?}}},"status":{{"phase":{phase:?},"containerStatuses":[{}]}}}}"#,
                    statuses.join(",")
                )
            })
            .collect();
        format!(r#"{{"items":[{}]}}"#, items.join(","))
    }

    /// Three pods in the default namespace and one in `orders`.
    fn seeded() -> FakeKubectl {
        let fake = FakeKubectl::new();
        fake.insert_list(
            "",
            "pods",
            pods_json(&[
                (
                    "api-7d9f",
                    "Running",
                    &[(true, 0, ""), (true, 2, "")],
                    "2026-01-02T01:04:05Z",
                ),
                (
                    "worker-5c2a",
                    "Running",
                    &[(false, 41, "CrashLoopBackOff")],
                    "2026-01-01T03:04:05Z",
                ),
                ("migrate-x1", "Succeeded", &[], "2026-01-02T03:00:05Z"),
            ]),
        );
        fake.insert_list(
            "orders",
            "pods",
            pods_json(&[(
                "orders-0",
                "Running",
                &[(true, 0, "")],
                "2026-01-02T02:04:05Z",
            )]),
        );
        fake.insert_list("empty", "pods", r#"{"items":[]}"#);
        fake
    }

    fn ctx_with(port: impl KubectlPort + 'static) -> Ctx {
        let mut ctx = ctx();
        ctx.deps.kubectl = Some(Arc::new(port));
        ctx
    }

    fn pod_list(ctx: Ctx) -> Shell {
        let screen = K8sPods::new(&ctx);
        Shell::new(ctx, Box::new(screen))
    }

    fn enter() -> KeyEvent {
        key(KeyCode::Enter)
    }

    /// The name of the pod the selection is on.
    fn selected(shell: &Shell) -> String {
        render_shell(shell, 80, 24)
            .lines()
            .find_map(|line| line.strip_prefix("> "))
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_default()
            .to_owned()
    }

    /// A port that fails every listing while `failing` is set.
    struct Flaky {
        inner: FakeKubectl,
        failing: Arc<AtomicBool>,
    }

    #[async_trait]
    impl KubectlPort for Flaky {
        async fn get_json(
            &self,
            namespace: &str,
            resource: &str,
            name: &str,
        ) -> Result<Vec<u8>, SvcError> {
            self.inner.get_json(namespace, resource, name).await
        }

        async fn list_json(&self, namespace: &str, resource: &str) -> Result<Vec<u8>, SvcError> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(SvcError::Other(anyhow::anyhow!(
                    "kubectl: the server is unreachable"
                )));
            }
            self.inner.list_json(namespace, resource).await
        }

        async fn logs(
            &self,
            namespace: &str,
            pod: &str,
            follow: bool,
        ) -> Result<mpsc::Receiver<String>, SvcError> {
            self.inner.logs(namespace, pod, follow).await
        }
    }

    #[tokio::test]
    async fn the_pod_list_opens_on_the_default_namespace() {
        let mut shell = pod_list(ctx_with(seeded()));

        let shown = settle(&mut shell, "api-7d9f").await;

        assert!(shown.contains("Pods — default namespace"), "{shown}");
        assert_eq!(selected(&shell), "api-7d9f");
    }

    #[tokio::test]
    async fn pods_are_listed_with_ready_status_restarts_and_age() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;

        insta::assert_snapshot!(render_shell(&shell, 80, 10));
    }

    #[tokio::test]
    async fn n_asks_for_a_namespace_and_lists_it() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;

        press(&mut shell, &[ch('n')]);
        assert!(render_shell(&shell, 80, 24).contains("namespace:"));
        press(&mut shell, &typed("orders"));
        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "orders-0").await;

        assert!(shown.contains("Pods — orders"), "{shown}");
        assert!(!shown.contains("api-7d9f"), "{shown}");
        assert!(
            !shown.contains("namespace:"),
            "the prompt has closed: {shown}"
        );
    }

    #[tokio::test]
    async fn letters_typed_into_the_namespace_are_not_commands() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;

        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("rn"));

        assert!(render_shell(&shell, 80, 24).contains("namespace: rn"));
    }

    #[tokio::test]
    async fn a_pasted_namespace_is_text_and_lists_nothing_until_enter() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;
        press(&mut shell, &[ch('n')]);

        shell.handle(None, Msg::Paste("orders\n".to_owned()));

        let shown = render_shell(&shell, 80, 24);
        assert!(shown.contains("namespace: orders"), "{shown}");
        assert!(shown.contains("Pods — default namespace"), "{shown}");
    }

    #[tokio::test]
    async fn an_empty_namespace_answer_returns_to_the_default() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;
        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("orders"));
        press(&mut shell, &[enter()]);
        settle(&mut shell, "orders-0").await;

        press(&mut shell, &[ch('n'), enter()]);
        let shown = settle(&mut shell, "api-7d9f").await;

        assert!(shown.contains("Pods — default namespace"), "{shown}");
    }

    #[tokio::test]
    async fn esc_closes_the_namespace_prompt_before_it_leaves() {
        let ctx = ctx_with(seeded());
        let mut screen = K8sPods::new(&ctx);
        screen.init(&ctx);
        screen.update(Msg::Key(ch('n')), &ctx);

        assert!(screen.update(Msg::Key(key(KeyCode::Esc)), &ctx).is_empty());
        assert!(!render_screen(&screen, 60, 6).contains("namespace:"));

        let cmds = screen.update(Msg::Key(key(KeyCode::Esc)), &ctx);
        assert!(matches!(cmds.as_slice(), [Cmd::Pop]));
    }

    #[tokio::test]
    async fn r_reloads_and_keeps_the_selected_pod() {
        let fake = seeded();
        let mut shell = pod_list(ctx_with(fake.clone()));
        settle(&mut shell, "api-7d9f").await;
        press(&mut shell, &[key(KeyCode::Down)]);
        assert_eq!(selected(&shell), "worker-5c2a");

        fake.insert_list(
            "",
            "pods",
            pods_json(&[
                (
                    "aaa-new",
                    "Pending",
                    &[(false, 0, "ContainerCreating")],
                    "2026-01-02T03:04:00Z",
                ),
                (
                    "worker-5c2a",
                    "Running",
                    &[(true, 42, "")],
                    "2026-01-01T03:04:05Z",
                ),
            ]),
        );
        press(&mut shell, &[ch('r')]);
        let shown = settle(&mut shell, "aaa-new").await;

        assert_eq!(selected(&shell), "worker-5c2a", "{shown}");
        assert!(!shown.contains("api-7d9f"), "{shown}");
    }

    #[tokio::test]
    async fn a_namespace_without_pods_says_no_pods() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;

        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("empty"));
        press(&mut shell, &[enter()]);

        let shown = settle(&mut shell, "no pods in this namespace").await;
        assert!(shown.contains("Pods — empty"), "{shown}");
    }

    #[tokio::test]
    async fn kubernetes_not_configured_shows_the_error_and_esc_leaves() {
        let ctx = ctx();
        let mut screen = K8sPods::new(&ctx);

        assert!(screen.init(&ctx).is_empty());

        let shown = render_screen(&screen, 60, 6);
        assert!(shown.contains("Kubernetes is not available"), "{shown}");
        assert!(matches!(
            screen.update(Msg::Key(key(KeyCode::Esc)), &ctx).as_slice(),
            [Cmd::Pop]
        ));
    }

    #[tokio::test]
    async fn a_first_listing_that_fails_shows_the_error_and_r_tries_again() {
        let failing = Arc::new(AtomicBool::new(true));
        let port = Flaky {
            inner: seeded(),
            failing: Arc::clone(&failing),
        };
        let mut shell = pod_list(ctx_with(port));
        let shown = settle(&mut shell, "the server is unreachable").await;
        assert!(shown.contains("error"), "{shown}");

        failing.store(false, Ordering::SeqCst);
        press(&mut shell, &[ch('r')]);

        settle(&mut shell, "api-7d9f").await;
    }

    #[tokio::test]
    async fn a_failed_reload_keeps_the_listing_and_reports_the_error() {
        let failing = Arc::new(AtomicBool::new(false));
        let port = Flaky {
            inner: seeded(),
            failing: Arc::clone(&failing),
        };
        let mut shell = pod_list(ctx_with(port));
        settle(&mut shell, "api-7d9f").await;

        failing.store(true, Ordering::SeqCst);
        press(&mut shell, &[ch('r')]);
        let shown = settle(&mut shell, "the server is unreachable").await;

        assert!(shown.contains("api-7d9f"), "{shown}");
    }

    #[tokio::test]
    async fn a_namespace_that_does_not_exist_is_reported_and_the_listing_stays() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;

        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("nowhere"));
        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "does not exist").await;

        assert!(
            shown.contains("namespace \"nowhere\" does not exist"),
            "{shown}"
        );
        assert!(shown.contains("Pods — default namespace"), "{shown}");
        assert!(shown.contains("api-7d9f"), "{shown}");
    }

    /// A cluster with no pods anywhere, where looking a namespace up is not
    /// allowed.
    struct Locked;

    #[async_trait]
    impl KubectlPort for Locked {
        async fn get_json(&self, _: &str, resource: &str, name: &str) -> Result<Vec<u8>, SvcError> {
            Err(SvcError::Other(anyhow::anyhow!(
                "kubectl get {resource} {name} -o json: Error from server (Forbidden): namespaces \"{name}\" is forbidden"
            )))
        }

        async fn list_json(&self, _: &str, _: &str) -> Result<Vec<u8>, SvcError> {
            Ok(br#"{"items":[]}"#.to_vec())
        }

        async fn logs(
            &self,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<mpsc::Receiver<String>, SvcError> {
            Err(SvcError::Other(anyhow::anyhow!("no logs here")))
        }
    }

    #[tokio::test]
    async fn an_empty_namespace_that_cannot_be_confirmed_says_so() {
        let mut shell = pod_list(ctx_with(Locked));
        // The context's own namespace has no name to look up.
        let shown = settle(&mut shell, "no pods in this namespace").await;
        assert!(!shown.contains("confirmed"), "{shown}");

        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("locked"));
        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "could not be confirmed").await;

        assert!(shown.contains("Pods — locked"), "{shown}");
        assert!(shown.contains("no pods found"), "{shown}");
        assert!(shown.contains("is forbidden"), "{shown}");
        assert!(!shown.contains("no pods in this namespace"), "{shown}");
    }

    #[tokio::test]
    async fn the_note_about_an_unconfirmed_namespace_goes_with_the_next_listing() {
        let mut shell = pod_list(ctx_with(Locked));
        settle(&mut shell, "no pods in this namespace").await;
        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("locked"));
        press(&mut shell, &[enter()]);
        settle(&mut shell, "could not be confirmed").await;

        press(&mut shell, &[ch('n'), enter()]);
        let shown = settle(&mut shell, "Pods — default namespace").await;

        assert!(shown.contains("no pods in this namespace"), "{shown}");
        assert!(!shown.contains("confirmed"), "{shown}");
    }

    #[test]
    fn a_stale_listing_is_ignored() {
        let ctx = ctx_with(seeded());
        let mut screen = K8sPods::new(&ctx);
        let first = match screen.init(&ctx).pop() {
            Some(Cmd::Task(id, _)) => id,
            _ => panic!("the list asks for the pods first"),
        };
        let second = match screen.update(Msg::Key(ch('r')), &ctx).pop() {
            Some(Cmd::Task(id, _)) => id,
            _ => panic!("r asks again"),
        };
        let pod = |name: &str| Pod {
            name: name.to_owned(),
            phase: "Running".to_owned(),
            status: "Running".to_owned(),
            ready: "1/1".to_owned(),
            restarts: 0,
            created: None,
        };

        screen.update(Msg::Loaded(first, Loaded::Pods(vec![pod("stale")])), &ctx);
        assert!(!render_screen(&screen, 60, 6).contains("stale"));

        screen.update(Msg::Loaded(second, Loaded::Pods(vec![pod("fresh")])), &ctx);
        assert!(render_screen(&screen, 60, 6).contains("fresh"));
    }

    #[tokio::test]
    async fn pod_names_with_control_characters_are_listed_safely() {
        let fake = FakeKubectl::new();
        fake.insert_list(
            "",
            "pods",
            r#"{"items":[{"metadata":{"name":"evil\u001b[2J\tpod"},"status":{"phase":"Run\tning"}}]}"#,
        );
        let mut shell = pod_list(ctx_with(fake));

        // The escape character goes and each tab becomes spaces up to the
        // next stop; a tab that reached the terminal would be dropped.
        let shown = settle(&mut shell, "evil[2J pod").await;

        assert!(shown.contains("Run     ning"), "{shown}");
        assert!(!shown.contains('\x1b'), "{shown:?}");
    }

    #[tokio::test]
    async fn the_selection_stays_in_view() {
        let pods: Vec<(String, &str)> = (0..60)
            .map(|i| (format!("pod-{i:02}"), "Running"))
            .collect();
        let listing: Vec<PodRow<'_>> = pods
            .iter()
            .map(|(name, phase)| (name.as_str(), *phase, &[][..], "2026-01-02T01:04:05Z"))
            .collect();
        let fake = FakeKubectl::new();
        fake.insert_list("", "pods", pods_json(&listing));
        let mut shell = pod_list(ctx_with(fake));
        settle(&mut shell, "pod-00").await;

        press(&mut shell, &[key(KeyCode::Down); 45]);
        assert_eq!(selected(&shell), "pod-45");

        press(&mut shell, &[ch('g')]);
        assert_eq!(selected(&shell), "pod-00");
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let ctx = ctx_with(seeded());
        let mut screen = K8sPods::new(&ctx);
        screen.init(&ctx);
        render_screen(&screen, 1, 1);
        render_screen(&screen, 0, 0);
        screen.update(Msg::Key(ch('n')), &ctx);
        render_screen(&screen, 1, 1);
    }

    #[tokio::test]
    async fn enter_on_a_pod_opens_its_log() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;
        press(&mut shell, &[key(KeyCode::Down), enter()]);
        assert_eq!(shell.depth(), 2);
        assert!(render_shell(&shell, 80, 24).contains("Logs — worker-5c2a"));

        // In a named namespace the title says which.
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;
        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("orders"));
        press(&mut shell, &[enter()]);
        settle(&mut shell, "orders-0").await;
        press(&mut shell, &[enter()]);
        assert!(render_shell(&shell, 80, 24).contains("Logs — orders/orders-0"));
    }

    #[tokio::test]
    async fn enter_on_an_empty_list_opens_nothing() {
        let mut shell = pod_list(ctx_with(seeded()));
        settle(&mut shell, "api-7d9f").await;
        press(&mut shell, &[ch('n')]);
        press(&mut shell, &typed("empty"));
        press(&mut shell, &[enter()]);
        settle(&mut shell, "no pods in this namespace").await;

        press(&mut shell, &[enter()]);

        assert_eq!(shell.depth(), 1);
    }
}
