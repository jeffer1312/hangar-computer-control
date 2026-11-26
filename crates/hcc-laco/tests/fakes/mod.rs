use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hcc_protocolo::{Action, ActResult, Connector, Delivery, Observation, Scope, Session, SessionError};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wiremock::{Request, Respond, ResponseTemplate};

pub struct State {
    pub views: VecDeque<Observation>,
    pub acts: Vec<(Action, String, Instant)>,
    pub observe_errors: VecDeque<SessionError>,
    pub act_errors: VecDeque<SessionError>,
    pub screenshot_error: Option<SessionError>,
    pub close_error: Option<String>,
    pub close_pending: bool,
    pub slow_drop: Option<Arc<SlowDrop>>,
    pub connect_error: Option<SessionError>,
    pub connect_pending: bool,
    pub observe_pending: bool,
    pub advance_on_act: bool,
    /// A failed act also moves to the next view (the screen changed or the ids were renumbered).
    pub advance_on_error: bool,
    /// One per successful act, in order; `delivered_unverified` once used up.
    pub act_deliveries: VecDeque<Delivery>,
    /// The scope of every observe, in order.
    pub scopes: Vec<Scope>,
    /// Every observe after the first moves to the next view.
    pub advance_on_observe: bool,
    /// The Nth observe (1-based) fails with this error.
    pub fail_observe_at: Option<(usize, SessionError)>,
    /// The window of every screenshot, in order.
    pub shot_windows: Vec<Option<String>>,
    /// Input mode and cursor of every act; `acts` keeps the action without them.
    pub modos: Vec<(Option<hcc_protocolo::InputMode>, bool)>,
    pub connects: usize,
    pub closes: usize,
    pub captures: usize,
    pub observed: Vec<Instant>,
    pub cancel_after_act: Option<CancellationToken>,
}

#[derive(Clone)]
pub struct FakeConnector {
    pub state: Arc<Mutex<State>>,
    pub entered: Arc<Notify>,
}

pub struct FakeSession(FakeConnector);

pub struct SlowDrop {
    pub entered: std::sync::mpsc::Sender<()>,
    pub release: Mutex<std::sync::mpsc::Receiver<()>>,
    pub finished: Notify,
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        let slow = self.0.state.lock().unwrap().slow_drop.clone();
        if let Some(slow) = slow {
            slow.entered.send(()).unwrap();
            slow.release.lock().unwrap().recv().unwrap();
            slow.finished.notify_one();
        }
    }
}

impl FakeConnector {
    pub fn new(views: Vec<Observation>) -> Self {
        assert!(!views.is_empty());
        Self {
            state: Arc::new(Mutex::new(State {
                views: views.into(), acts: vec![], observe_errors: VecDeque::new(), act_errors: VecDeque::new(),
                screenshot_error: None, close_error: None, close_pending: false, slow_drop: None, connect_error: None,
                connect_pending: false, observe_pending: false, advance_on_act: true, advance_on_error: false,
                act_deliveries: VecDeque::new(), scopes: vec![], advance_on_observe: false, fail_observe_at: None, shot_windows: vec![], modos: vec![],
                connects: 0, closes: 0, captures: 0, observed: vec![], cancel_after_act: None,
            })),
            entered: Arc::new(Notify::new()),
        }
    }
}

impl Connector for FakeConnector {
    type S = FakeSession;

    async fn connect(&self, _: &CancellationToken) -> Result<Self::S, SessionError> {
        let (pending, error) = {
            let mut s = self.state.lock().unwrap();
            s.connects += 1;
            (s.connect_pending, s.connect_error.clone())
        };
        self.entered.notify_one();
        if pending { std::future::pending::<()>().await; }
        match error {
            Some(e) => Err(e),
            None => Ok(FakeSession(self.clone())),
        }
    }
}

impl Session for FakeSession {
    async fn observe(&mut self, scope: &Scope) -> Result<Observation, SessionError> {
        let pending = {
            let mut s = self.0.state.lock().unwrap();
            s.scopes.push(scope.clone());
            if let Some((n, e)) = s.fail_observe_at.clone() && n == s.scopes.len() { return Err(e); }
            if s.advance_on_observe && s.scopes.len() > 1 && s.views.len() > 1 { s.views.pop_front(); }
            s.observe_pending
        };
        self.0.entered.notify_one();
        if pending { std::future::pending::<()>().await; }
        let mut s = self.0.state.lock().unwrap();
        s.observed.push(Instant::now());
        match s.observe_errors.pop_front() {
            Some(e) => Err(e),
            None => Ok(s.views.front().unwrap().clone()),
        }
    }

    async fn act(&mut self, id: &str, action: &Action) -> Result<ActResult, SessionError> {
        let mut s = self.0.state.lock().unwrap();
        assert_eq!(id, s.views.front().unwrap().observation_id);
        s.modos.push((action.input, action.cursor));
        s.acts.push((Action { input: None, cursor: false, ..action.clone() }, id.into(), Instant::now()));
        if let Some(c) = &s.cancel_after_act { c.cancel(); }
        if let Some(e) = s.act_errors.pop_front() {
            if s.advance_on_error && s.views.len() > 1 { s.views.pop_front(); }
            return Err(e);
        }
        if s.advance_on_act && s.views.len() > 1 { s.views.pop_front(); }
        Ok(ActResult { ok: true, delivery: s.act_deliveries.pop_front().unwrap_or(Delivery::DeliveredUnverified) })
    }

    async fn screenshot(&mut self, window: Option<&str>) -> Result<Vec<u8>, SessionError> {
        let mut s = self.0.state.lock().unwrap();
        s.captures += 1;
        s.shot_windows.push(window.map(str::to_owned));
        match &s.screenshot_error {
            Some(e) => Err(e.clone()),
            None => Ok(png_1280x720()),
        }
    }

    async fn close(&mut self) -> Result<(), String> {
        let (pending, error) = {
            let mut s = self.0.state.lock().unwrap();
            s.closes += 1;
            (s.close_pending, s.close_error.clone())
        };
        if pending { std::future::pending::<()>().await; }
        error.map_or(Ok(()), Err)
    }
}

#[derive(Clone)]
pub enum Answer {
    Choice(&'static str, f64, f64),
    Select(&'static str, f64, f64),
    Risk(f64),
    Vision(Value),
    /// Answers the `pick` question with the criterion containing this text ("NONE" picks none).
    Pick(&'static str, f64),
    /// HTTP 400 to every request carrying the screenshot (`state.tela`), the inner answer to the first without it.
    SemTela(Box<Answer>),
    /// The inner answer plus `blocked` with this probability.
    Bloqueio(Box<Answer>, f64),
}

#[derive(Clone)]
pub struct Script(pub Arc<Mutex<VecDeque<Answer>>>);

impl Script {
    pub fn new(answers: Vec<Answer>) -> Self { Self(Arc::new(Mutex::new(answers.into()))) }
}

impl Respond for Script {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let mut fila = self.0.lock().unwrap();
        if matches!(fila.front(), Some(Answer::SemTela(_))) && body["state"].get("tela").is_some() {
            return ResponseTemplate::new(400).set_body_string("image not supported");
        }
        // Jev's pick among the helper's proposals, unscripted: NONE, so the loop goes as it did without it.
        if body["questions"]["pick"]["instructions"] == hcc_laco::prompts::PROPOSTA && !matches!(fila.front(), Some(Answer::Pick(..))) {
            return ResponseTemplate::new(200).set_body_json(json!({"answers": {"pick": {"choice": "NONE", "probabilities": {"NONE": 1.0}}}}));
        }
        let (answer, bloqueio) = match fila.pop_front().expect("unexpected model request") {
            Answer::SemTela(a) => (*a, None),
            Answer::Bloqueio(a, b) => (*a, Some(b)),
            a => (a, None),
        };
        drop(fila);
        let choice = |choice: &str, p: f64, risk: f64| json!({"answers": {
            "action": {"choice": choice, "probabilities": {choice: p}}, "risky": {"noul": risk},
        }});
        let mut response = match answer {
            Answer::Choice(c, p, risk) => choice(c, p, risk),
            Answer::Select(description, p, risk) => {
                let criteria = body["questions"]["action"]["criteria"].as_object().unwrap();
                let index = criteria.iter().find(|(_, v)| v.as_str().is_some_and(|v| v.contains(description)))
                    .unwrap_or_else(|| panic!("missing criterion {description}: {criteria:?}")).0;
                choice(index, p, risk)
            }
            Answer::Risk(risk) => {
                assert!(body["questions"].get("action").is_none());
                json!({"answers": {"risky": {"noul": risk}}})
            }
            Answer::Vision(help) => json!({"choices": [{"message": {"content": help.to_string()}}]}),
            Answer::Pick(texto, p) => json!({"answers": {"pick": pick(&body, texto, p)}}),
            Answer::SemTela(_) | Answer::Bloqueio(..) => panic!("wrappers do not nest"),
        };
        if let Some(b) = bloqueio { response["answers"]["blocked"] = json!({"noul": b}); }
        if response.get("answers").is_some() {
            response["model"] = json!("jev-1.14.0");
            response["usage"] = json!({"input_tokens": 1000});
        }
        ResponseTemplate::new(200).set_body_json(response)
    }
}

/// The `pick` answer choosing the criterion that contains `texto` ("NONE" picks none).
fn pick(body: &Value, texto: &str, p: f64) -> Value {
    let criteria = body["questions"]["pick"]["criteria"].as_object().unwrap_or_else(|| panic!("no pick question: {body}"));
    let k = if texto == "NONE" { "NONE".to_owned() } else {
        criteria.iter().find(|(k, v)| *k != "NONE" && v.as_str().is_some_and(|v| v.contains(texto)))
            .unwrap_or_else(|| panic!("missing pick {texto}: {criteria:?}")).0.clone()
    };
    json!({"choice": k, "probabilities": {k.clone(): p}})
}

pub fn tree(id: &str, value: &str) -> Observation {
    serde_json::from_value(json!({
        "observation_id": id, "connected": true, "session_id": 1, "foreground": "w1",
        "windows": [{"id": "w1", "name": "Editor", "process_id": 10, "class_name": "TMainForm", "rect": [0,0,800,600]}],
        "elements": [
            {"id":"e0", "name":"Nome", "role":"Edit", "value":value, "enabled":true,
             "rect":null, "focused":true, "actions":["set_value"]},
            {"id":"e1", "name":"Salvar", "role":"Button", "value":null, "enabled":true,
             "rect":null, "focused":false, "actions":["invoke"]},
        ],
        "truncated":false, "timestamp":1.0, "screen":{"width":800,"height":600},
    })).unwrap()
}

pub fn help(actions: Value) -> Answer {
    Answer::Vision(json!({"interpretacao":"tela vista", "acoes": actions}))
}

pub fn seconds(n: u64) -> Duration { Duration::from_secs(n) }

/// A white 1280x720 PNG, encoded once.
pub fn png_1280x720() -> Vec<u8> {
    static PNG: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(1280, 720, image::Rgba([255, 255, 255, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        png
    });
    PNG.clone()
}
