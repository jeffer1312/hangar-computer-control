# hangar-computer-control

MCP que opera um desktop Windows a partir de um objetivo em português. O chamador (Claude, Codex, Hangar)
manda "abrir o sistema e fazer login" e recebe de volta "concluído" ou "parou: motivo". Ninguém no meio.

Como funciona, por ciclo:

1. O agente na sessão gráfica do Windows lê a árvore de acessibilidade (UI Automation) da janela ativa,
   dos popups dela e da barra de tarefas.
2. O laço transforma a árvore em ações concretas (clicar botão X, digitar valor Y no campo Z, ativar janela W).
3. O Jev (TypeSafe) escolhe uma ação. Não gera texto.
4. Só quando o Jev não acha saída, ou a árvore vem vazia, um LLM barato olha o print e propõe ações extras,
   que voltam pro Jev escolher. Janela da frente sem nenhum controle na árvore (botões desenhados, como a
   caixa de mensagem da TMS): o print vai direto ao LLM, que devolve o clique rotulado ("botão Não"); o laço
   executa sem votação, só depois da pergunta de risco ao Jev e se o clique cair dentro dessa janela.
5. O agente executa, o laço observa de novo. Repete até DONE, impedimento real ou limite.

Não tem regra por aplicativo. Tela nova é só árvore nova.

## O que precisa

No controlador (esta máquina, Linux ou Windows):

- [uv](https://docs.astral.sh/uv/) (ele baixa o Python 3.13 e as dependências sozinho).
- `TYPESAFE_API_KEY` no ambiente (Jev).
- `LLM_PROXY_KEY` no ambiente e, se não for o padrão, `LLM_PROXY_URL` (padrão `http://127.0.0.1:8317/v1/chat/completions`)
  e `LLM_MODEL` (padrão `gpt-5.6-luna`). `LLM_EFFORT` (`low`/`medium`/`high`) vai como `reasoning_effort`;
  vazio não envia. Qualquer endpoint compatível com OpenAI `/v1/chat/completions` serve. Só é usado no fallback com imagem.
- `HCC_AGENT_CONFIG` apontando pro JSON que diz onde está o Windows (abaixo).

No Windows controlado:

- Usuário logado numa sessão gráfica **ativa**. Sessão RDP desconectada ou tela bloqueada faz o agente
  recusar (`sessão Windows desconectada`). Pra manter ativa sem cliente RDP: `tscon <id> /dest:console`.
- Pra controle remoto: SSH com chave (PowerShell ou cmd como shell) e permissão de criar tarefa agendada
  interativa. O agente sobe elevado (`RunLevel Highest`).
- Nada de Python lá: o `windows-agent.exe` já carrega tudo (PyInstaller). Baixe da página de releases
  (`https://github.com/jeffer1312/hangar-computer-control/releases`), gerado a cada tag pelo GitHub Actions.

## Arquivo de configuração do agente (`HCC_AGENT_CONFIG`)

Remoto por SSH (caso normal):

```json
{
  "transport": "ssh",
  "host": "minha-vm",
  "agent_path": "/caminho/para/windows-agent.exe",
  "request_timeout": 40
}
```

`host` é um alias do `~/.ssh/config`; `agent_path` é o exe baixado, nesta máquina. O exe é copiado uma vez
(SCP ou `shared_executable`, um caminho UNC visível de lá) e verificado por SHA-256 a cada conexão.
`request_timeout` sobe pra 40 em IDE pesada (Delphi trava a UIA por segundos). `proxy_command` é opcional.
Modelo: `exemplo-agent.json`.

Na mesma máquina Windows: `{"transport": "local", "command": ["C:\\HangarComputerControl\\windows-agent.exe"]}`.

Vários Windows: cada um é um `<nome>-agent.json` na pasta `HCC_AGENTS_DIR` (ausente = pasta do
`HCC_AGENT_CONFIG`). As ferramentas aceitam `alvo="<nome>"`; sem ele, vale o `HCC_AGENT_CONFIG`. A lista
de alvos na descrição das ferramentas é lida quando o MCP sobe.

## Registrar o MCP

O Hangar faz isso pela tela Configurações > Controle do Windows. À mão, no `~/.claude.json`:

```json
{"mcpServers": {"hangar-computer-control": {
  "command": "uvx",
  "args": ["--from", "git+https://github.com/jeffer1312/hangar-computer-control@v0.1.0", "hangar-computer-control"],
  "env": {"HCC_AGENT_CONFIG": "/caminho/minha-vm-agent.json", "TYPESAFE_API_KEY": "…",
          "LLM_PROXY_URL": "…", "LLM_PROXY_KEY": "…", "LLM_MODEL": "…"}}}}
```

Ou `claude mcp add` com os mesmos valores.

## Ferramentas

Todas aceitam `alvo` (nome de um `<nome>-agent.json`); sem ele, vale o `HCC_AGENT_CONFIG`.

- `objetivo(texto, dados={}, max_passos=12, limite_segundos=240)`: a principal. `texto` é o pedido como
  se fosse pra uma pessoa. `dados` leva valores que o laço pode digitar (usuário, senha, caminho, texto):
  qualquer valor em `dados` vira candidato de "digitar isso em cada campo editável". Retorna um texto com
  `concluído: …` ou `parou: …`, a lista de passos executados com a confiança do Jev, os tempos por etapa,
  o caminho do último print (se o LLM foi chamado) e a pasta de registro (`observacao.json`, `jev.jsonl`).
- `ver_tela()`: print do desktop, devolve o caminho do PNG. Pra quando `objetivo` parou e você quer olhar.
- `estado()`: diz se o desktop está acessível e a resolução.

Exemplos de `texto` que funcionaram: "abrir o Bloco de Notas, digitar o texto informado e salvar o arquivo
no caminho informado" (com `dados={"texto": …, "caminho": …}`); "no Chrome, no GitLab da empresa, abrir
a lista de merge requests do projeto X"; "abrir Configurações do Windows e ir em Sistema > Sobre".

## Quando ele para sozinho

- Ação arriscada (apagar, sobrescrever, enviar, fechar sem salvar) que o texto do objetivo não autorizou.
  Repita o objetivo dizendo explicitamente ("…substituindo se já existir").
- A tela pede algo que não foi dado (login, credencial de rede): devolve isso como impedimento.
- O mesmo aviso do aplicativo volta duas vezes: devolve o texto do aviso.
- Três ações seguidas sem efeito, ou `max_passos`/`limite_segundos`.

## Limites conhecidos

- Texto de console (Windows Terminal) ainda não é lido: comando roda, mas o resultado não é conferido.
- Leitura da árvore em IDE Delphi leva 10 a 13 s por ciclo (VCL via MSAA); no resto, 1 s.
- Um objetivo por vez por Windows alvo (trava na pasta temporária, por usuário e por alvo).

## Desenvolver

- Suíte local, sem Windows: `uv run python -m unittest -q`.
- Recompilar o agente num Windows: `scripts/build-windows-agent.ps1`. Tag `v*` publicada faz o mesmo no
  GitHub Actions e anexa o exe à release.
- Detalhes do transporte remoto e empacotamento: `DESKTOP.md`.
