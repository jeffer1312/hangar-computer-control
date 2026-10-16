//! Pieces (format version 3): reusable runs of steps with `{pN}` slots for what changes between uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use hcc_protocolo::ActionType;

use hcc_protocolo::Observation;
use serde_json::{Map, Value};

use crate::roteiro::{Alvo, Passo, Valor, resolver_com, salvar_json, slug};

pub const VERSAO_TRECHO: u32 = 3;
const MAX_VARIANTES: usize = 5;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Parametro {
    /// The value seen when it was recorded (never a secret): helps Jev and people read the description.
    pub exemplo: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VarianteTrecho {
    pub passos: Vec<Passo>,
    #[serde(default)]
    pub usos: u32,
}

/// A reusable run of steps. `contexto` is the window class of its first step, or, when it starts by opening an
/// app, the class of the window that app brought to the front (web: the pathname).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Trecho {
    pub versao: u32,
    pub tipo: String,
    pub descricao: String,
    #[serde(default)]
    pub parametros: BTreeMap<String, Parametro>,
    pub contexto: String,
    /// Alternative ways of the same piece, latest first: the first whose first step fits the screen is replayed.
    pub variantes: Vec<VarianteTrecho>,
}

fn normal(texto: &str) -> String {
    texto.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// A context with ids (a route like /paciente/123, a class with a build number) is the same context for any id.
pub fn contexto_normal(contexto: &str) -> String {
    let mut s = String::new();
    for c in contexto.chars() { if c.is_ascii_digit() { if !s.ends_with('0') { s.push('0'); } } else { s.push(c); } }
    s
}

pub fn arquivo(pasta: &Path, descricao: &str, contexto: &str) -> PathBuf {
    let semente = json!([normal(descricao), contexto_normal(contexto)]).to_string();
    let hash: String = Sha256::digest(semente.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
    let slug = Some(slug(descricao)).filter(|s| !s.is_empty()).unwrap_or_else(|| "trecho".to_owned());
    pasta.join(format!("trecho-{slug}-{hash}.json"))
}

/// Every readable piece of the folder; files of another version or kind are skipped (they may be newer).
pub fn todos(pasta: &Path) -> Vec<(PathBuf, Trecho)> {
    let Ok(lista) = std::fs::read_dir(pasta) else { return vec![]; };
    let mut achados: Vec<(PathBuf, Trecho)> = lista.filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("trecho-") && n.ends_with(".json")))
        .filter_map(|p| {
            let t: Trecho = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
            (t.versao == VERSAO_TRECHO && t.tipo == "trecho").then_some((p, t))
        })
        .collect();
    achados.sort_by(|a, b| a.0.cmp(&b.0));
    achados
}

/// The new way goes first; a way with the same steps leaves, carrying its use count over.
pub fn juntar(anterior: Option<Trecho>, novo: Trecho) -> Trecho {
    let mesmo = |a: &[Passo], b: &[Passo]| a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| x.janela == y.janela && x.alvo == y.alvo && x.acao == y.acao && x.valor == y.valor);
    let Some(mut base) = anterior else {
        let mut novo = novo;
        for v in &mut novo.variantes { v.usos = v.usos.max(1); }
        return novo;
    };
    for mut v in novo.variantes {
        let usos = base.variantes.iter().find(|a| mesmo(&a.passos, &v.passos)).map_or(0, |a| a.usos.max(1));
        base.variantes.retain(|a| !mesmo(&a.passos, &v.passos));
        v.usos = v.usos.max(1) + usos;
        base.variantes.insert(0, v);
    }
    base.variantes.truncate(MAX_VARIANTES);
    base
}

pub fn salvar(arquivo: &Path, trecho: &Trecho) -> std::io::Result<()> {
    salvar_json(arquivo, trecho)
}

/// `{pN}` replaced by its value; `{{` and `}}` stand for literal braces.
pub fn preencher(modelo: &str, params: &BTreeMap<String, String>) -> Option<String> {
    let mut saida = String::new();
    let mut resto = modelo;
    while let Some(i) = resto.find(['{', '}']) {
        saida.push_str(&resto[..i]);
        let c = &resto[i..];
        if let Some(r) = c.strip_prefix("{{") { saida.push('{'); resto = r; continue; }
        if let Some(r) = c.strip_prefix("}}").or_else(|| c.strip_prefix('}')) { saida.push('}'); resto = r; continue; }
        let fim = c.find('}')?;
        saida.push_str(params.get(&c[1..fim])?);
        resto = &c[fim + 1..];
    }
    saida.push_str(resto);
    Some(saida)
}

/// Window titles carry counters ("4 novos itens"): any run of digits matches any run of digits.
pub fn mesmo_titulo(gravado: &str, agora: &str) -> bool {
    fn sem_numeros(t: &str) -> String {
        let mut s = String::new();
        for c in t.chars() { if c.is_ascii_digit() { if !s.ends_with('#') { s.push('#'); } } else { s.push(c); } }
        s
    }
    sem_numeros(gravado) == sem_numeros(agora)
}

/// A piece before its description is written: steps already templated, the slots it uses with their examples.
#[derive(Clone, Debug, PartialEq)]
pub struct Rascunho {
    pub contexto: String,
    pub parametros: BTreeMap<String, Parametro>,
    pub passos: Vec<Passo>,
}

/// Cuts a run into pieces and turns into slots the goal's values that the run acted with.
/// A cut falls where the front window changes, or where a step that changed the screen also changed the title
/// (counters aside). A goal value is a slot only when the run acted with it (a control's name, a typed text);
/// one seen only in titles (the app's name) stays fixed. Titles are not kept: a piece may start on any screen of
/// its window, so it is matched by window class and controls.
// ponytail: value found by plain substring, longest first; a name that also prints on an unrelated control is
// taken as a slot there too, and replay's per-step check is the net.
pub fn cortar(passos: &[Passo], valores: &[String], classe_final: Option<&str>) -> Vec<Rascunho> {
    let escapar = |t: &str| t.replace('{', "{{").replace('}', "}}");
    let nomes = |p: &Passo| -> Vec<String> { match &p.alvo {
        Some(Alvo::Elemento { name, text, placeholder, .. }) => [Some(name), text.as_ref(), placeholder.as_ref()].into_iter().flatten().cloned().collect(),
        _ => vec![],
    } };
    let literal = |p: &Passo| match &p.valor { Some(Valor::Literal(v)) => Some(v.clone()), _ => None };
    let mut variaveis: Vec<&String> = valores.iter().filter(|v| v.chars().count() >= 3)
        .filter(|v| passos.iter().any(|p| nomes(p).iter().any(|n| n.contains(v.as_str())) || literal(p).is_some_and(|l| l.contains(v.as_str()))))
        .collect();
    variaveis.sort_by_key(|v| std::cmp::Reverse(v.chars().count()));
    let mut inicios = vec![0];
    for i in 0..passos.len().saturating_sub(1) {
        let (a, b) = (&passos[i], &passos[i + 1]);
        if a.janela.classe != b.janela.classe || (a.mudou_tela == Some(true) && !mesmo_titulo(&a.janela.titulo, &b.janela.titulo)) {
            inicios.push(i + 1);
        }
    }
    inicios.iter().enumerate().map(|(n, &ini)| {
        let fim = inicios.get(n + 1).copied().unwrap_or(passos.len());
        let mut parametros = BTreeMap::new();
        let slot = |v: &String, parametros: &mut BTreeMap<String, Parametro>| {
            let achado = parametros.iter().find(|(_, p)| &p.exemplo == v).map(|(k, _)| k.clone());
            achado.unwrap_or_else(|| {
                let k = format!("p{}", parametros.len() + 1);
                parametros.insert(k.clone(), Parametro { exemplo: v.clone() });
                k
            })
        };
        let trecho: Vec<Passo> = passos[ini..fim].iter().map(|p| {
            let mut p = p.clone();
            p.janela.titulo = String::new();
            if let Some(Alvo::Elemento { name, text, placeholder, .. }) = &mut p.alvo {
                for campo in [Some(name), text.as_mut(), placeholder.as_mut()].into_iter().flatten() {
                    let mut t = escapar(campo);
                    for v in &variaveis {
                        let e = escapar(v);
                        if t.contains(&e) { t = t.replace(&e, &format!("{{{}}}", slot(v, &mut parametros))); }
                    }
                    *campo = t;
                }
            }
            if let Some(Valor::Literal(v)) = &p.valor
                && let Some(var) = variaveis.iter().find(|x| x.as_str() == v) {
                p.valor = Some(Valor::Parametro(slot(var, &mut parametros)));
            }
            p
        }).collect();
        let contexto = if trecho.first().is_some_and(|p| p.acao.kind == ActionType::Launch) {
            passos.get(ini + 1).map(|p| p.janela.classe.clone()).or_else(|| classe_final.map(str::to_owned)).unwrap_or_default()
        } else { trecho.first().map(|p| p.janela.classe.clone()).unwrap_or_default() };
        Rascunho { contexto, parametros, passos: trecho }
    // A piece with no context would fit everywhere: an app opener would open the app again and again.
    }).filter(|r| !r.contexto.is_empty()).collect()
}

/// A piece that fits the screen now, with the goal values in its slots.
#[derive(Clone, Debug, PartialEq)]
pub struct Opcao {
    pub texto: String,
    pub trecho: usize,
    pub variante: usize,
    pub params: BTreeMap<String, String>,
}

/// Pieces whose first step resolves on this screen, each slot filled with one of the goal's values. A piece that
/// opens an app fits only while no window of that app exists; any other fits only in a window of its context.
// ponytail: every binding of the goal's values is tried (capped); fine for the few slots a piece has.
pub fn opcoes(pecas: &[(PathBuf, Trecho)], obs: &Observation, dados: &Map<String, Value>, valores: &[String]) -> Vec<Opcao> {
    let frente = obs.windows.iter().find(|w| w.id == obs.foreground).map(|w| contexto_normal(&w.class_name)).unwrap_or_default();
    let valores: Vec<&String> = valores.iter().take(12).collect();
    let mut saida: Vec<Opcao> = Vec::new();
    for (t, (_, peca)) in pecas.iter().enumerate() {
        let abre = peca.variantes.iter().any(|v| v.passos.first().is_some_and(|p| p.acao.kind == ActionType::Launch));
        let cabe = if abre { !obs.windows.iter().any(|w| contexto_normal(&w.class_name) == contexto_normal(&peca.contexto)) }
            else { contexto_normal(&peca.contexto) == frente };
        if !cabe { continue; }
        let chaves: Vec<&String> = peca.parametros.keys().collect();
        let mut ligacoes: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
        for k in &chaves {
            ligacoes = ligacoes.into_iter().flat_map(|l| valores.iter().map(move |v| {
                let mut l = l.clone(); l.insert((*k).clone(), (*v).clone()); l
            })).take(200).collect();
        }
        for params in ligacoes {
            let Some(v) = peca.variantes.iter().position(|v| v.passos.first().is_some_and(|p| resolver_com(p, obs, dados, &params).is_some()))
                else { continue; };
            let Some(texto) = preencher(&peca.descricao, &params) else { continue; };
            if saida.iter().any(|o| o.texto == texto) || saida.len() >= 200 { continue; }
            saida.push(Opcao { texto, trecho: t, variante: v, params });
        }
    }
    saida
}

/// A step as it ran: `tela` identifies the screen right before it (a hash of the screen signature).
#[derive(Clone, Debug)]
pub struct Feito {
    pub passo: Passo,
    pub ok: bool,
    pub tela: u64,
}

/// The steps worth learning, by index: ran ok and changed the screen, and were not undone later. Undone: a typing
/// erased right after (select all, delete) or typed again into the same field; something opened and closed at once
/// by a cancel, close or Escape that brought back the screen it was opened from.
pub fn bons(feitos: &[Feito], tela_final: u64) -> Vec<usize> {
    let tela_depois = |i: usize| feitos.get(i + 1).map_or(tela_final, |f| f.tela);
    let mut ficam: Vec<usize> = (0..feitos.len()).filter(|&i| feitos[i].ok && feitos[i].passo.mudou_tela == Some(true)).collect();
    let digita = |p: &Passo| matches!(p.acao.kind, ActionType::SetValue | ActionType::Text) && matches!(p.alvo, Some(Alvo::Elemento { .. }));
    let teclas = |p: &Passo| p.acao.keys.as_deref().unwrap_or_default().iter().map(|k| k.to_lowercase()).collect::<Vec<_>>();
    let apaga = |p: &Passo| p.acao.kind == ActionType::Keys && teclas(p).iter().any(|k| k == "delete" || k == "backspace");
    let cancela = |p: &Passo| match (&p.alvo, p.acao.kind) {
        (_, ActionType::Keys) => teclas(p) == ["escape"],
        (Some(Alvo::Elemento { role, name, .. }), ActionType::Invoke | ActionType::Mouse) => role == "Button" && {
            let n: String = name.to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect();
            ["cancelar", "cancel", "fechar", "close", "não", "nao", "no"].contains(&n.trim())
        },
        _ => false,
    };
    let mut fora = vec![false; ficam.len()];
    for a in 0..ficam.len() {
        let p = &feitos[ficam[a]].passo;
        if fora[a] { continue; }
        if digita(p) {
            // Retyped in the same field: only the last typing counts.
            if ficam[a + 1..].iter().take(3).any(|&j| digita(&feitos[j].passo) && feitos[j].passo.alvo == p.alvo) { fora[a] = true; continue; }
            // Erased by keys right after: the typing and the keys up to the delete go.
            let seguintes: Vec<usize> = (a + 1..ficam.len()).take(3).take_while(|&b| feitos[ficam[b]].passo.acao.kind == ActionType::Keys).collect();
            if let Some(&fim) = seguintes.iter().find(|&&b| apaga(&feitos[ficam[b]].passo)) {
                for b in a..=fim { fora[b] = true; }
            }
            continue;
        }
        if let Some(b) = (a + 1 < ficam.len()).then_some(a + 1)
            && cancela(&feitos[ficam[b]].passo) && !cancela(p) && tela_depois(ficam[b]) == feitos[ficam[a]].tela {
            fora[a] = true; fora[b] = true;
        }
    }
    let mut n = 0;
    ficam.retain(|_| { n += 1; !fora[n - 1] });
    ficam
}

/// The piece's description: its steps, slots kept. Deterministic, so the same way always lands in the same file.
pub fn resumo(passos: &[Passo]) -> String {
    passos.iter().map(|p| {
        let alvo = match &p.alvo {
            Some(Alvo::Elemento { role, name, .. }) => format!(" {role} '{name}'"),
            Some(Alvo::Janela(j)) => format!(" window '{}'", j.classe),
            None => String::new(),
        };
        let valor = match &p.valor {
            Some(Valor::Parametro(k)) => format!(" '{{{k}}}'"),
            Some(Valor::Dados(k)) => format!(" <dados.{k}>"),
            Some(Valor::Literal(v)) => format!(" '{}'", v.replace('{', "{{").replace('}', "}}")),
            None => String::new(),
        };
        let extra = match p.acao.kind {
            ActionType::Keys => format!(" {}", p.acao.keys.as_deref().unwrap_or_default().join("+").to_lowercase()),
            ActionType::Launch => format!(" {}", p.acao.application.as_deref().unwrap_or_default().rsplit(['/', '\\']).next().unwrap_or_default()),
            ActionType::Scroll => format!(" {}", p.acao.direction.map_or("", crate::descrever::direcao)),
            _ => String::new(),
        };
        format!("{}{alvo}{extra}{valor}", p.acao.kind)
    }).collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roteiro::{Janela, Ordem};
    use serde_json::json;
    use hcc_protocolo::Action;

    fn clique(nome: &str) -> Passo {
        Passo { janela: Janela { classe: "app".into(), titulo: "App".into() },
            alvo: Some(Alvo::Elemento { role: "Button".into(), name: nome.into(), text: None, placeholder: None, test_id: None,
                ordem: Ordem { indice: 0, de: 1 } }),
            acao: Action { kind: ActionType::Invoke, ..Default::default() }, valor: None, mudou_tela: Some(true) }
    }

    fn trecho(passos: Vec<Passo>) -> Trecho {
        Trecho { versao: VERSAO_TRECHO, tipo: "trecho".into(), descricao: "abrir {p1}".into(),
            parametros: BTreeMap::from([("p1".into(), Parametro { exemplo: "Ana".into() })]), contexto: "app".into(),
            variantes: vec![VarianteTrecho { passos, usos: 0 }] }
    }

    fn passo_em(classe: &str, titulo: &str, acao: Action, alvo: Option<&str>, valor: Option<Valor>, mudou: bool) -> Passo {
        Passo { janela: Janela { classe: classe.into(), titulo: titulo.into() },
            alvo: alvo.map(|n| Alvo::Elemento { role: "ListItem".into(), name: n.into(), text: None, placeholder: None, test_id: None,
                ordem: Ordem { indice: 0, de: 1 } }),
            acao, valor, mudou_tela: Some(mudou) }
    }

    // Generic: open an app, open an item by a name from the goal, search a term from the goal.
    #[test]
    fn a_run_is_cut_where_the_window_or_its_title_changes_and_goal_values_become_slots() {
        let abrir = Action { kind: ActionType::Launch, application: Some("/usr/bin/app".into()), ..Default::default() };
        let atalho = Action { kind: ActionType::Keys, keys: Some(vec!["ctrl".into(), "f".into()]), ..Default::default() };
        let texto = Action { kind: ActionType::Text, ..Default::default() };
        let invoke = Action { kind: ActionType::Invoke, ..Default::default() };
        let corrida = vec![
            passo_em("term", "~ - fish", abrir, None, None, true),
            passo_em("app", "geral - App 4", invoke.clone(), Some("Junior, Mensagem direta"), None, true),
            passo_em("app", "Junior - App 4", atalho, None, None, true),
            passo_em("app", "Junior - App 5", texto, None, Some(Valor::Literal("OpenRouter".into())), true),
        ];
        let pedacos = cortar(&corrida, &["OpenRouter".into(), "Junior".into(), "App".into()], None);
        assert_eq!(pedacos.len(), 3, "{pedacos:#?}");
        assert_eq!((pedacos[0].contexto.as_str(), pedacos[0].passos.len(), pedacos[0].parametros.len()), ("app", 1, 0),
            "opening the app: its context is the window it brought up");
        assert_eq!(pedacos[1].contexto, "app");
        assert!(matches!(&pedacos[1].passos[0].alvo, Some(Alvo::Elemento { name, .. }) if name == "{p1}, Mensagem direta"));
        assert_eq!(pedacos[1].parametros["p1"].exemplo, "Junior");
        assert_eq!(pedacos[2].passos.len(), 2, "the shortcut and the typing stay together: the title kept its counter only");
        assert_eq!(pedacos[2].passos[1].valor, Some(Valor::Parametro("p1".into())));
        assert_eq!(pedacos[2].parametros["p1"].exemplo, "OpenRouter");
        assert!(pedacos.iter().flat_map(|p| &p.passos).all(|p| p.janela.titulo.is_empty()), "pieces do not compare titles");
        assert!(!pedacos.iter().any(|p| p.parametros.values().any(|v| v.exemplo == "App")), "a name only in titles is not a slot");
    }

    // The same task by two ways: click the item when it is on screen, search for it otherwise.
    #[test]
    fn the_visible_item_way_fits_only_when_the_item_is_on_screen() {
        let abrir_item = passo_em("app", "", Action { kind: ActionType::Invoke, ..Default::default() }, Some("{p1}"), None, true);
        let buscar = vec![
            passo_em("app", "", Action { kind: ActionType::Keys, keys: Some(vec!["ctrl".into(), "k".into()]), ..Default::default() }, None, None, true),
            passo_em("app", "", Action { kind: ActionType::Text, ..Default::default() }, None, Some(Valor::Parametro("p1".into())), true),
        ];
        let peca = |passos: Vec<Passo>| {
            let mut t = trecho(passos.clone());
            t.descricao = resumo(&passos);
            (PathBuf::from("x"), t)
        };
        let pecas = vec![peca(vec![abrir_item]), peca(buscar)];
        let tela = |itens: &[&str]| -> Observation { serde_json::from_value(json!({
            "observation_id": "o", "connected": true, "session_id": 1, "foreground": "w1",
            "windows": [{"id": "w1", "name": "App", "process_id": 1, "class_name": "app", "rect": [0,0,10,10]}],
            "elements": itens.iter().enumerate().map(|(i, n)| json!({"id": format!("e{i}"), "name": n, "role": "ListItem",
                "value": null, "enabled": true, "rect": null, "focused": false, "actions": ["invoke"]})).collect::<Vec<_>>(),
            "truncated": false, "timestamp": 1.0, "screen": {"width": 10, "height": 10}})).unwrap() };
        let textos = |obs: &Observation| opcoes(&pecas, obs, &Map::new(), &["Rafael".into()]).into_iter().map(|o| o.texto).collect::<Vec<_>>();
        assert_eq!(textos(&tela(&["Junior", "Rafael"])), ["invoke ListItem 'Rafael'", "keys ctrl+k; text 'Rafael'"]);
        assert_eq!(textos(&tela(&["Junior"])), ["keys ctrl+k; text 'Rafael'"]);
        let mut outra = tela(&["Rafael"]);
        outra.windows[0].class_name = "outra".into();
        assert!(textos(&outra).is_empty(), "a piece fits only in a window of its context");
    }

    // A run that only opened the app: the window in front at the end is the app it opened.
    #[test]
    fn opening_the_app_as_the_last_piece_takes_the_final_window_or_is_dropped() {
        let abrir = || vec![passo_em("term", "~", Action { kind: ActionType::Launch, application: Some("calc".into()), ..Default::default() }, None, None, true)];
        let fim = cortar(&abrir(), &[], Some("org.gnome.Calculator"));
        assert_eq!(fim.iter().map(|p| p.contexto.as_str()).collect::<Vec<_>>(), ["org.gnome.Calculator"]);
        assert!(cortar(&abrir(), &[], None).is_empty(), "without knowing the app's window it would reopen it every time");
    }

    #[test]
    fn description_is_the_steps_with_their_slots() {
        let corrida = vec![
            passo_em("app", "", Action { kind: ActionType::Invoke, ..Default::default() }, Some("{p1}, Mensagem direta"), None, true),
            passo_em("app", "", Action { kind: ActionType::Keys, keys: Some(vec!["CTRL".into(), "F".into()]), ..Default::default() }, None, None, true),
            passo_em("app", "", Action { kind: ActionType::Text, ..Default::default() }, None, Some(Valor::Parametro("p1".into())), true),
        ];
        assert_eq!(resumo(&corrida), "invoke ListItem '{p1}, Mensagem direta'; keys ctrl+f; text '{p1}'");
    }

    #[test]
    fn pieces_live_in_their_own_files_and_ways_accumulate_with_use() {
        let pasta = std::env::temp_dir().join(format!("hcc-trecho-{}", std::process::id()));
        std::fs::create_dir_all(&pasta).unwrap();
        let nome = arquivo(&pasta, "Abrir {p1}", "app");
        assert!(nome.file_name().unwrap().to_str().unwrap().starts_with("trecho-abrir-p1-"));
        assert_eq!(nome, arquivo(&pasta, "abrir  {p1}", "app"), "same piece whatever the spacing or case");
        assert_eq!(arquivo(&pasta, "x", "/paciente/123"), arquivo(&pasta, "x", "/paciente/9"), "ids in the context are one context");
        let direto = trecho(vec![clique("{p1}")]);
        let busca = trecho(vec![clique("Buscar"), clique("{p1}")]);
        let r = juntar(Some(juntar(Some(direto.clone()), busca)), direto);
        assert_eq!(r.variantes.len(), 2, "the visible-item way and the search way");
        assert_eq!((r.variantes[0].passos.len(), r.variantes[0].usos), (1, 2), "used twice, now first");
        salvar(&nome, &r).unwrap();
        std::fs::write(pasta.join("trecho-novo-1.json"), r#"{"versao":4,"tipo":"trecho"}"#).unwrap();
        std::fs::write(pasta.join("desktop-x-1.json"), "{}").unwrap();
        assert_eq!(todos(&pasta).into_iter().map(|(_, t)| t).collect::<Vec<_>>(), vec![r]);
        std::fs::remove_dir_all(pasta).unwrap();
    }

    fn p(pares: &[(&str, &str)]) -> BTreeMap<String, String> { pares.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }

    #[test]
    fn slots_are_filled_and_braces_escaped() {
        assert_eq!(preencher("{p1}, Mensagem direta", &p(&[("p1", "Rafael")])).as_deref(), Some("Rafael, Mensagem direta"));
        assert_eq!(preencher("{{x}} {p1}", &p(&[("p1", "a")])).as_deref(), Some("{x} a"));
        assert_eq!(preencher("{p2}", &p(&[("p1", "a")])), None, "a slot without value does not resolve");
        assert_eq!(preencher("sem lacuna", &p(&[])).as_deref(), Some("sem lacuna"));
    }

    #[test]
    fn titles_match_across_counters_only() {
        assert!(mesmo_titulo("Junior (MD) - Acme - 4 novos itens - Slack", "Junior (MD) - Acme - 12 novos itens - Slack"));
        assert!(!mesmo_titulo("Junior (MD) - Acme - Slack", "Rafael (MD) - Acme - Slack"));
    }
}
