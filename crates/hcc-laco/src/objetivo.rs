//! What the goal text itself gives the loop: values to type, a reading request, a misheard name.

use std::collections::HashSet;
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
    Regex::new(r"(?i)\b(digit|escrev|preench|insir|inser|inform|coloc|pesquis|busc|procur|respond|envi|mand|type|write|fill|enter|search|input)\w*").unwrap()
});
static NOME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\p{Lu}\p{L}{3,}\b").unwrap());
static PALAVRA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{L}{3,}").unwrap());

/// Texts the goal spells out (quoted or URLs): typing them is not inventing data.
pub fn literais(goal: &str) -> Vec<String> {
    let mut vistos = HashSet::new();
    // A quoted text names a row, a button, a page unless the goal asks to type something.
    let digitar = DIGITAR.is_match(&crate::candidatos::afirmado(goal));
    let citados = LITERAIS.captures_iter(goal).filter(|_| digitar)
        .filter_map(|c| c.iter().skip(1).flatten().next()).map(|m| m.as_str().to_owned());
    let urls = URL.find_iter(goal).map(|m| m.as_str().trim_end_matches(['.', ',', ';', ':', ')', '!', '?']).to_owned());
    citados.chain(urls).filter(|t| t.chars().count() >= 2 && vistos.insert(t.clone())).collect()
}

pub fn pede_leitura(goal: &str) -> bool {
    LEITURA.is_match(goal) && !ACAO_DIRIGIDA.is_match(goal)
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

    // "messages" named a table row and was typed in the site's search box.
    #[test]
    fn citacao_so_vira_texto_quando_o_objetivo_pede_para_digitar() {
        assert!(literais("ligar o interruptor da linha \"messages\". NÃO usar a caixa \"Pesquisar\"").is_empty());
        assert_eq!(literais("na página \"X\", ir para https://y.dev/a"), vec!["https://y.dev/a"]);
        assert_eq!(literais("preencher o campo \"URL\" com 'abc'"), vec!["URL", "abc"]);
    }
}
