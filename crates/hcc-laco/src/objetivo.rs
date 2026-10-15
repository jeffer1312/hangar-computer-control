//! What the goal text itself gives the loop: values to type, a reading request, a misheard name.

use std::collections::HashSet;

use serde_json::{Map, Value};
use std::sync::LazyLock;

use hcc_protocolo::Observation;
use regex::Regex;

use crate::barreiras::LITERAIS;

static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://[^\s'"“”<>]+"#).unwrap());
static LEITURA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(ler|leia|leiam|lê|lendo|read|mostre|mostra|resuma|resumir)\b").unwrap()
});
// Sending, deleting, scheduling, prescribing or buying aims at someone/something exact: never resolve a misheard
// name for it, and such a goal is not a plain reading either.
static ACAO_DIRIGIDA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(envi|mand|respond|escrev|digit|apag|exclu|delet|remov|send|post|publi|compr|pag|curt|segu|bloque|denunc|agend|cancel|marc|transf|lig|cham|prescrev|assin|aprov|autoriz)\w*").unwrap()
});
static DIGITAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:(?:digit|escrev|preench|pesquis|procur)\w*|insir[ao]|inserir|insere|inform(?:e|ar|a)|coloqu?(?:e|ar|a)|busqu?(?:e|ar|a)|envi(?:e|ar|a)|mand(?:e|ar|a)|respond(?:a|er|e)|type|write|fill|enter|search|input)\b").unwrap()
});
static NOME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\p{Lu}\p{L}{3,}\b").unwrap());
static PALAVRA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{L}{3,}").unwrap());

/// Texts the goal spells out (quoted or URLs): typing them is not inventing data.
pub fn literais(goal: &str) -> Vec<String> {
    let mut vistos = HashSet::new();
    // A quoted text names a row, a button, a page unless the goal asks to type something.
    let digitar = pede_digitar(goal);
    let citados = LITERAIS.captures_iter(goal).filter(|_| digitar)
        .filter_map(|c| c.iter().skip(1).flatten().next()).map(|m| m.as_str().to_owned());
    let urls = URL.find_iter(goal).map(|m| m.as_str().trim_end_matches(['.', ',', ';', ':', ')', '!', '?']).to_owned());
    citados.chain(urls).filter(|t| t.chars().count() >= 2 && vistos.insert(t.clone())).collect()
}

pub(crate) fn pede_digitar(goal: &str) -> bool {
    // A participle ("já preenchido", "digitado antes") describes the screen, it asks for nothing.
    let participio = |w: &str| ["ado", "ada", "ados", "adas", "ido", "ida", "idos", "idas"].iter().any(|f| w.to_lowercase().ends_with(f));
    DIGITAR.find_iter(&crate::candidatos::afirmado(goal)).any(|m| !participio(m.as_str()))
}

pub fn pede_leitura(goal: &str) -> bool {
    LEITURA.is_match(goal) && !ACAO_DIRIGIDA.is_match(goal)
}

/// What in the goal may change from one use to the next: quoted texts, URLs, proper names (a run of them too),
/// the term after a search verb, and the non-secret values of `dados`. Pieces turn these into slots.
pub fn valores_do_objetivo(goal: &str, dados: &Map<String, Value>, segredos: &HashSet<String>) -> Vec<String> {
    static BUSCA: LazyLock<Regex> = LazyLock::new(|| Regex::new(
        r"(?i)\b(?:pesquis|busc|procur|search|find)\w*\s+(?:(?:por|pelo|pela|o|a|os|as|for|the)\s+)?([^\s,.;:!?]{3,})").unwrap());
    let inicio = |i: usize| goal[..i].trim_end().is_empty() || goal[..i].trim_end().ends_with(['.', '!', '?', ':']);
    let nomes: Vec<regex::Match> = NOME.find_iter(goal).filter(|m| !inicio(m.start())).collect();
    let mut valores: Vec<String> = LITERAIS.captures_iter(goal).filter_map(|c| c.iter().skip(1).flatten().next()).map(|m| m.as_str().to_owned())
        .chain(URL.find_iter(goal).map(|m| m.as_str().trim_end_matches(['.', ',', ';', ':', ')', '!', '?']).to_owned()))
        .chain(BUSCA.captures_iter(goal).filter_map(|c| c.get(1)).map(|m| m.as_str().to_owned()))
        .chain(nomes.iter().map(|m| m.as_str().to_owned()))
        .collect();
    // "Rafael Freitas": names separated by a single space are also one name.
    for i in 0..nomes.len() {
        let mut fim = i;
        while fim + 1 < nomes.len() && goal[nomes[fim].end()..nomes[fim + 1].start()] == *" " { fim += 1; }
        if fim > i { valores.push(goal[nomes[i].start()..nomes[fim].end()].to_owned()); }
    }
    valores.extend(dados.values().map(crate::candidatos::py_str));
    let mut vistos = HashSet::new();
    valores.into_iter().filter(|v| v.chars().count() >= 3 && !segredos.contains(v) && vistos.insert(v.clone())).collect()
}

/// A reading goal whose affirmed part asks no action: the screen is read and returned, nothing is clicked.
pub fn so_leitura(goal: &str) -> bool {
    static ACAO: LazyLock<Regex> = LazyLock::new(|| Regex::new(
        r"(?i)\b(abr|clic|clique|rol|ir\b|vá|va\b|entr|pesquis|busc|procur|digit|escrev|preench|selecion|naveg|volt|fech|apert|press|ativ|toc|marc|lig|deslig|expand|envi|mand|open|click|scroll|go\b|search|type|press|select)\w*",
    ).unwrap());
    // Any change the goal asks (delete, save, copy, reply...) is an action: only a goal that just reads skips Jev.
    static MUDA: LazyLock<Regex> = LazyLock::new(|| Regex::new(
        r"(?i)\b(encaminh|limp|arquiv|salv|copi|col[ae]|cri[ae]|edit|alter|mud|renome|mov|instal|desinstal|baix|carreg|imprim|exporte|import|save|copy|paste|create|delete|close|reply|write|edit|move|print|download|upload|forward|clear|archive|remove|send)\w*",
    ).unwrap());
    let afirmado = crate::candidatos::afirmado(goal);
    LEITURA.is_match(&afirmado) && !ACAO.is_match(&afirmado) && !ACAO_DIRIGIDA.is_match(&afirmado) && !MUDA.is_match(&afirmado)
}

/// Visible text of the front window, in tree order, once each: what a reading goal returns.
pub fn texto_janela(obs: &Observation) -> String {
    let mut vistos = HashSet::new();
    let mut partes = Vec::new();
    for e in &obs.elements {
        if !matches!(e.role.as_str(), "Text" | "Group" | "ListItem" | "Hyperlink" | "Article" | "Heading") { continue; }
        let t = e.value.as_deref().filter(|v| !vazio(v)).unwrap_or(&e.name).trim();
        if !vazio(t) && t.chars().count() >= 2 && vistos.insert(t.to_owned()) { partes.push(t.to_owned()); }
    }
    let texto = partes.join(" | ");
    // ponytail: fixed cap; the caller reads it aloud or summarises it, it is not an export.
    if texto.chars().count() > 2000 { texto.chars().take(2000).collect::<String>() + "…" } else { texto }
}

fn vazio(t: &str) -> bool {
    t.chars().all(|c| c.is_whitespace() || c == '\u{fffc}')
}

fn normal(s: &str) -> String {
    s.to_lowercase().chars().map(|c| match c {
        'á' | 'à' | 'â' | 'ã' | 'ä' => 'a', 'é' | 'ê' | 'è' | 'ë' => 'e', 'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o', 'ú' | 'ù' | 'û' | 'ü' => 'u', 'ç' => 'c', c => c,
    }).collect()
}

fn distancia(a: &[char], b: &[char]) -> usize {
    let mut linha: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diag = linha[0];
        linha[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let antes = linha[j + 1];
            linha[j + 1] = (diag + usize::from(ca != cb)).min(linha[j] + 1).min(linha[j + 1] + 1);
            diag = antes;
        }
    }
    linha[b.len()]
}

/// A capitalised goal word that is on no control, while exactly one visible name is a small spelling away
/// ("Juno" from speech, "Junior" on screen): the hint names it so navigation and reading go on.
// ponytail: same first two letters and at most a third of the longer word changed; a second close name means
// real ambiguity, so no hint and the loop asks.
/// Every proper name of the goal (not a sentence-opening verb) is a word of the front window's title: the open
/// conversation or document is on the tree, not only in the print. Never for goals that send, delete or pay.
pub fn nomes_no_titulo(goal: &str, obs: &Observation) -> bool {
    obs.windows.iter().find(|w| w.id == obs.foreground).is_some_and(|w| nomes_em(goal, &w.name))
}

/// The same evidence from a window that appeared during the run: an app opened without taking the focus.
pub fn nomes_em_janela_nova(goal: &str, obs: &Observation, iniciais: &HashSet<String>) -> bool {
    obs.windows.iter().filter(|w| !iniciais.contains(&w.id)).any(|w| nomes_em(goal, &w.name))
}

fn nomes_em(goal: &str, titulo: &str) -> bool {
    if ACAO_DIRIGIDA.is_match(goal) { return false; }
    let palavras: HashSet<String> = PALAVRA.find_iter(titulo).map(|m| normal(m.as_str())).collect();
    let inicio = |i: usize| goal[..i].trim_end().is_empty() || goal[..i].trim_end().ends_with(['.', '!', '?', ':']);
    let nomes: Vec<String> = NOME.find_iter(goal).filter(|m| !inicio(m.start())).map(|m| normal(m.as_str())).collect();
    !nomes.is_empty() && nomes.iter().all(|n| palavras.contains(n))
}

pub fn nome_parecido(goal: &str, obs: &Observation) -> Option<(String, String)> {
    if ACAO_DIRIGIDA.is_match(goal) { return None; }
    let nomes: Vec<&str> = obs.elements.iter().map(|e| e.name.as_str())
        .chain(obs.windows.iter().map(|w| w.name.as_str())).collect();
    let palavras: HashSet<String> = nomes.iter().flat_map(|n| PALAVRA.find_iter(n).map(|m| m.as_str().to_owned())).collect();
    let presentes: HashSet<String> = palavras.iter().map(|p| normal(p)).collect();
    // A capitalised word opening a sentence is a verb ("Abra o menu"), not a name.
    let inicio = |i: usize| goal[..i].trim_end().is_empty() || goal[..i].trim_end().ends_with(['.', '!', '?', ':']);
    for dita in NOME.find_iter(goal).filter(|m| !inicio(m.start())).map(|m| m.as_str()) {
        let alvo: Vec<char> = normal(dita).chars().collect();
        if presentes.contains(&alvo.iter().collect::<String>()) { continue; }
        let mut perto: Vec<&String> = palavras.iter().filter(|p| {
            let p: Vec<char> = normal(p).chars().collect();
            let maior = p.len().max(alvo.len());
            p.len() >= 3 && p[..2] == alvo[..2] && distancia(&alvo, &p) * 3 <= maior
        }).collect();
        perto.sort_by_key(|p| normal(p));
        perto.dedup_by_key(|p| normal(p));
        if let [unico] = perto[..] {
            return Some((dita.to_owned(), unico.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distancia_conta_edicoes() {
        let d = |a: &str, b: &str| distancia(&a.chars().collect::<Vec<_>>(), &b.chars().collect::<Vec<_>>());
        assert_eq!(d("juno", "junior"), 2);
        assert_eq!(d("abra", "abrir"), 2);
        assert_eq!(d("", "ab"), 2);
    }

    #[test]
    fn literais_pegam_citacao_e_url_sem_pontuacao_final() {
        assert_eq!(literais("digite https://x.com, depois 'olá' e \"ok\"."), vec!["olá", "ok", "https://x.com"]);
    }

    #[test]
    fn goal_values_that_may_change_between_uses() {
        let dados = serde_json::json!({"nome": "Bia", "senha": "x9"}).as_object().unwrap().clone();
        let segredos = std::collections::HashSet::from(["x9".to_owned()]);
        let v = valores_do_objetivo("Abra a conversa com Rafael Freitas e pesquise openrouter, depois \"DevOps\" em https://x.dev/a", &dados, &segredos);
        for esperado in ["DevOps", "https://x.dev/a", "Rafael Freitas", "Rafael", "Freitas", "openrouter", "Bia"] {
            assert!(v.contains(&esperado.to_owned()), "{esperado} in {v:?}");
        }
        assert!(!v.iter().any(|x| x == "Abra" || x == "x9"), "a sentence-opening verb and a secret are never values: {v:?}");
    }

    // "messages" named a table row and was typed in the site's search box.
    #[test]
    fn citacao_so_vira_texto_quando_o_objetivo_pede_para_digitar() {
        assert!(literais("ligar o interruptor da linha \"messages\". NÃO usar a caixa \"Pesquisar\"").is_empty());
        assert_eq!(literais("na página \"X\", ir para https://y.dev/a"), vec!["https://y.dev/a"]);
        assert_eq!(literais("preencher o campo \"URL\" com 'abc'"), vec!["URL", "abc"]);
    }

    // "o formulário já está preenchido" describes the screen; it asks nothing to be typed.
    #[test]
    fn a_participle_is_not_a_request_to_type() {
        assert!(!pede_digitar("Clicar em \"Criar Página\" (o formulário já está preenchido, dados digitados antes)"));
        assert!(pede_digitar("preencher o nome e digite a senha"));
    }
}
