# hangar-computer-control

MCP que opera um desktop Windows a partir de um objetivo em português. O chamador (Claude, Codex, Hangar)
manda "abrir o sistema e fazer login" e recebe de volta "concluído" ou "parou: motivo". Ninguém no meio.

Como funciona, por ciclo:

1. O agente na sessão gráfica do Windows lê a árvore de acessibilidade (UI Automation) da janela ativa,
   dos popups dela e da barra de tarefas.
2. O laço transforma a árvore em ações concretas (clicar botão X, digitar valor Y no campo Z, ativar janela W).
3. O Jev (TypeSafe) escolhe uma ação. Não gera texto.
4. Só quando o Jev não acha saída, ou a árvore vem vazia, um LLM barato olha o print e propõe ações extras,
   que voltam pro Jev escolher. Se o Jev continua em dúvida, a primeira ação proposta pelo LLM é executada,
   depois da pergunta de risco ao Jev. O pedido ao LLM leva o schema em `response_format` (modo estrito); endpoint
   que responder HTTP 400 recebe o pedido de novo sem ele, e o resto do objetivo segue sem o campo. Resposta do LLM sem o JSON é pedida de novo uma vez; ação inválida dela
   é descartada sozinha (mouse sem botão/modo vira clique esquerdo). Janela da frente sem nenhum controle na árvore (botões desenhados, como a
   caixa de mensagem da TMS): o print vai direto ao LLM, que devolve o clique rotulado ("botão Não"); o laço
   executa sem votação, só depois da pergunta de risco ao Jev e se o clique cair dentro dessa janela.
5. O agente executa, o laço observa de novo. Repete até DONE, impedimento real ou limite.

Não tem regra por aplicativo. Tela nova é só árvore nova.

## O que precisa

No controlador (esta máquina, Linux ou Windows):

- O binário `hangar-computer-control`, baixado da página de releases
  (`https://github.com/jeffer1312/hangar-computer-control/releases`): `hangar-computer-control-linux-x86_64`
  no Linux (renomeie e dê `chmod +x`), `windows-agent.exe` no Windows. Sem argumentos ele é o MCP (stdio);
  `hangar-computer-control agent --config <arquivo>` é o agente. Não precisa de Python nem de outro runtime.
- `TYPESAFE_API_KEY` no ambiente (Jev) e, se não for o padrão, `JEV_ENDPOINT` (padrão `https://api.typesafe.ai/v1/systemone`)
  e `JEV_MODEL` (padrão `jev-latest`). Vazio usa o padrão.
- `LLM_PROXY_KEY` no ambiente e, se não for o padrão, `LLM_PROXY_URL` (padrão `http://127.0.0.1:8317/v1/chat/completions`)
  e `LLM_MODEL` (padrão `gpt-5.6-luna`). `LLM_EFFORT` (`low`/`medium`/`high`) vai como `reasoning_effort`;
  vazio não envia. Qualquer endpoint compatível com OpenAI `/v1/chat/completions` serve. Só é usado no fallback com imagem.
- `HCC_AGENT_CONFIG` apontando pro JSON que diz onde está o Windows (abaixo).

No Windows controlado:

- Usuário logado numa sessão gráfica **ativa**. Sessão RDP desconectada ou tela bloqueada faz o agente
  recusar (`sessão Windows desconectada`). Pra manter ativa sem cliente RDP: `tscon <id> /dest:console`.
- Pra controle remoto: SSH com chave (PowerShell ou cmd como shell) e permissão de criar tarefa agendada
  interativa. O agente sobe elevado (`RunLevel Highest`).
- Nada de Python lá: o `windows-agent.exe` é o mesmo binário Rust compilado pra Windows, sem dependência.
  Baixe da página de releases (`https://github.com/jeffer1312/hangar-computer-control/releases`), gerado a
  cada tag pelo GitHub Actions.

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

Linux com Hyprland (Wayland), na mesma máquina: não precisa de arquivo. Com o MCP rodando na sessão
Hyprland, o alvo `linux` aparece sozinho e vira o padrão quando `HCC_AGENT_CONFIG` não está definido; ele
usa o próprio binário do MCP como agente (`agent --config`), com a configuração gravada em
`~/.cache/hangar-computer-control/linux-agent.v2.json`. Um `linux-agent.json` na pasta de alvos substitui o
automático; modelo no `linux-agent.json` do repositório.
Requisitos do sistema e detalhes em `DESKTOP.md`.

Vários Windows: cada um é um `<nome>-agent.json` na pasta `HCC_AGENTS_DIR` (ausente = pasta do
`HCC_AGENT_CONFIG`). As ferramentas aceitam `alvo="<nome>"`; sem ele, vale o `HCC_AGENT_CONFIG`. A lista
de alvos na descrição das ferramentas é lida quando o MCP sobe.

## Registrar o MCP

O Hangar faz isso pela tela Configurações > Controle do Windows. À mão, no `~/.claude.json`:

```json
{"mcpServers": {"hangar-computer-control": {
  "command": "/caminho/para/hangar-computer-control",
  "args": [],
  "env": {"HCC_AGENT_CONFIG": "/caminho/minha-vm-agent.json", "TYPESAFE_API_KEY": "…",
          "JEV_ENDPOINT": "…", "JEV_MODEL": "…",
          "LLM_PROXY_URL": "…", "LLM_PROXY_KEY": "…", "LLM_MODEL": "…"}}}}
```

Ou `claude mcp add` com os mesmos valores.

## Ferramentas

Todas aceitam `alvo` (nome de um `<nome>-agent.json`); sem ele, vale o `HCC_AGENT_CONFIG`.

- `objetivo(texto, dados={}, max_passos=300, limite_segundos=14400)`: a principal. Segue até concluir, até um
  impedimento real ou até ficar sem progresso; os dois limites são só tetos contra travamento, passe-os apenas
  para cortar antes. `texto` é o pedido como
  se fosse pra uma pessoa. `dados` leva valores que o laço pode digitar (usuário, senha, caminho, texto):
  qualquer valor em `dados` vira candidato de "digitar isso em cada campo editável", e o mesmo vale para
  URL escrita no próprio `texto`, e texto entre aspas quando o objetivo pede para digitar, preencher ou buscar
  (fora disso, aspas só nomeiam o controle ou a página). Na conferência final só se cobra texto entre aspas de
  objetivo que pede digitar, e só se ele não aparece na tela nem foi usado por uma ação da corrida (digitado ou
  clicado); texto citado em trecho negado ("não clicar em 'X'") e nome de controle ou janela da primeira tela nunca
  são cobrados. Pedido para agir num controle citado que continua na tela sem nenhuma ação feita: o "concluído" é
  recusado uma vez; se o Jev insistir, volta `parou: não confirmado: nada foi feito ainda…` (quem chamou decide). Depois de cada digitação num campo
  com nome, o laço confere o valor do campo na tela seguinte: valor diferente vira falha da ação (não progresso),
  o próximo passo passa pela ajuda visual e o motivo final diz com que valor o campo ficou. Controles de mesmo papel e nome (um interruptor
  por linha de tabela) aparecem pro Jev com o rótulo da linha: `Button 'X' [linha: messages]`. No Linux o
  documento da página rola pela roda do mouse (`scroll` up/down); com um diálogo aberto por cima, a roda vai no
  diálogo. Um caminho de arquivo em `dados` vira a opção de duplo clique no arquivo com esse nome em qualquer
  lista (seletor de arquivos KDE/GTK/Windows). Controle sem nome não ganha clique por coordenada. Retorna um texto com
  `concluído: …` ou `parou: …`, a lista de passos executados com a confiança do Jev, os tempos por etapa,
  o caminho do último print (se o LLM foi chamado) e a pasta de registro. Nela, por ciclo: `observacao-N.json` (a
  árvore) e `tela-N.png` (o print, quando o LLM foi chamado); em `jev.jsonl`, cada decisão com as opções oferecidas
  ao Jev e a dica, as propostas do LLM, a lista de ações com o resultado de cada uma (`acoes`) e o desfecho
  (`resultado`). É o que se lê para entender por que um objetivo parou.
  Objetivo de leitura ("leia", "mostre", "diga") devolve no `concluído:` o que foi lido: a leitura do print
  (`Leitura: …`) e/ou o texto visível da janela pela árvore (`Texto na janela: …`, até 2000 caracteres).
  `refazer=true` ignora o roteiro gravado (abaixo).
- `ver_tela()`: print do desktop, devolve o caminho do PNG. Pra quando `objetivo` parou e você quer olhar.
- `estado()`: diz se o desktop está acessível e a resolução.

Exemplos de `texto` que funcionaram: "abrir o Bloco de Notas, digitar o texto informado e salvar o arquivo
no caminho informado" (com `dados={"texto": …, "caminho": …}`); "no Chrome, no GitLab da empresa, abrir
a lista de merge requests do projeto X"; "abrir Configurações do Windows e ir em Sistema > Sobre".

### Roteiros (gravar e repetir)

Objetivo que termina `concluído` vira roteiro em `~/.cache/hangar-computer-control/roteiros/desktop-<objetivo>-<hash>.json`,
ou na pasta de `HCC_ROTEIROS` quando definida (para versionar num repositório, aponte para ele de propósito). Nunca no
repositório de quem chamou.
A chave é o texto do objetivo (sem diferença de maiúsculas e espaços) e os nomes das chaves de `dados`, sem máquina nem alvo;
os valores podem mudar entre execuções. Cada passo guarda a janela da frente (classe e título), o controle por
papel, nome e ordem entre iguais (`{indice, de}`: se a tela tiver outro número de iguais, diverge), e o valor como
referência à chave de `dados` (o valor em si nunca vai pro disco). O formato (`versao: 2`) é o mesmo do
`browser_goal` do Hangar, que usa também `text`, `placeholder`, `test_id` e `fim`; aqui ficam vazios ou são
ignorados. Arquivo da versão 1 é lido como uma variação; de outra versão é reportado como ilegível e o objetivo
segue pelo Jev.

O arquivo guarda até 5 variações que deram certo (`variantes`, a mais recente primeiro), porque a tela muda entre
instalações (versão do app no título, outro build). Na repetição vale a primeira variação cujo passo 1 bate com a
tela já lida; a escolha é feita antes de agir e não troca no meio. Ao concluir, o caminho da corrida vira a
variação 1 e uma variação com os mesmos passos sai.

Pedido dito com outras palavras: sem arquivo para o texto exato, o Jev escolhe entre os objetivos gravados na
pasta (só os que esta chamada consegue repetir, com as chaves de `dados` que eles usam) qual é a mesma tarefa, ou
nenhum; vale a escolha com 80% ou mais, e o resultado diz `parecido com '<objetivo gravado>'`.

Objetivo que parou (por qualquer motivo, inclusive erro): os passos que deram certo viram variação `parcial`
(depois das completas), sem perguntar ao Jev. Passo que deu certo = executado, com efeito na tela e sem
divergência no campo; saem também os desfeitos (texto digitado e apagado em seguida, campo redigitado, algo
aberto e fechado logo depois por Cancelar/Fechar/Esc). Cada passo bom entra na hora em
`<pasta dos roteiros>/diario/diario.jsonl`, para não se perder se o processo cair. Na repetição a parcial é
refeita e o Jev segue dali; quando o objetivo concluir, as variações parciais saem.

Na próxima chamada igual, o laço repete os passos sem perguntar ao Jev: lê a tela, acha o controle, executa.
Para na primeira divergência (janela diferente, controle ausente, ação recusada, tela que não mudou onde mudou
na gravação) e segue pelo Jev a partir da tela atual. O fim sempre passa pela confirmação normal do Jev; se o
objetivo concluir, o roteiro é regravado. O resultado traz `roteiro: variação K de V, repetidos N de M passos; gravado em …`
e os passos repetidos aparecem com `[roteiro]`. Apagar o arquivo ou `refazer=true` força o caminho pelo Jev.

- Clique por coordenada é gravado no menor controle com nome que estava embaixo e repetido no centro atual
  desse controle; clique sem controle com nome embaixo não se repete (devolve ao Jev ali).
- Passo repetido passa pela mesma regra de confirmação da escolha normal: confirmação de diálogo ou ação
  destrutiva que o objetivo atual não pede para a repetição e devolve ao Jev.
- Abrir um app é pulado quando a janela do passo seguinte já existe (não abre segunda instância).
- Roteiro em que qualquer campo traria um valor secreto de `dados` (texto do objetivo, título, rótulo da visão)
  não é gravado; o resultado diz `roteiro: não gravado: contém um valor secreto`.
- Toda ação bem-sucedida da corrida entra no roteiro, inclusive desvios que o Jev corrigiu depois (menu errado
  seguido de Esc); `refazer=true` regrava pelo caminho atual.
- Cada passo repetido ainda lê a árvore inteira, então no Delphi o ganho é só o do Jev e da visão.

### Pedaços reaproveitáveis (formato versão 3)

Além do roteiro do objetivo, toda corrida (concluída ou não, só com os passos que deram certo) é cortada em
pedaços que servem a outros objetivos, em qualquer app: abrir um app, abrir um item pelo nome, pesquisar um termo,
preencher um diálogo (campo, upload, caixa, salvar). Ficam em
`<pasta dos roteiros>/trechos/trecho-<descrição>-<hash>.json`, formato combinado com o gravador do navegador do Hangar:

```json
{"versao": 3, "tipo": "trecho", "descricao": "invoke ListItem '{p1}'", "parametros": {"p1": {"exemplo": "Ana"}},
 "contexto": "app", "variantes": [{"passos": [...], "usos": 2}]}
```

- Corte: onde a janela da frente muda, onde um diálogo abre ou fecha (cada diálogo é uma sub-tarefa), ou onde um
  passo que mudou a tela também mudou o título (contadores à parte).
- Forma (`forma`): papéis e palavras dos controles do diálogo do primeiro passo (ou da janela), sem os valores do
  objetivo. O pedaço só é oferecido numa tela de forma parecida (70% ou mais dos itens em comum), não pela frase do
  objetivo. Entre controles iguais, vale o que está dentro do diálogo aberto.
- Lacuna `{pN}`: um valor do objetivo (entre aspas, nome próprio, URL, termo depois de "pesquisar/buscar/procurar",
  valor não secreto de `dados`) com que a corrida agiu: nome do controle usado ou texto digitado. Valor visto só no
  título (o nome do app) fica fixo. No passo, `valor` pode ser `{"parametro": "pN"}`; nomes de controle levam o modelo
  (`{{`/`}}` para chave literal).
- `descricao` é o resumo dos passos com as lacunas, sem modelo de linguagem: o mesmo caminho cai sempre no mesmo
  arquivo e soma `usos`. `contexto` é a classe da janela do primeiro passo (do app aberto, quando o pedaço abre um
  app; no navegador, o caminho da página), com dígitos tratados como um só (`/paciente/123` = `/paciente/9`).
- Pedaço não guarda título: pode começar em qualquer tela da janela, e é conferido pela classe e pelos controles.
- Repetição: no primeiro ciclo (sem roteiro para o objetivo) e sempre que um diálogo abre ou fecha ou outra janela
  vem para a frente, as opções são os pedaços cujo primeiro passo cabe na tela atual com as
  lacunas preenchidas pelos valores do objetivo (pedaço que abre um app só cabe se o app não estiver aberto). O Jev
  escolhe uma delas ou nenhuma (vale com 80% ou mais); cada passo é conferido, e a primeira divergência devolve ao
  Jev. Depois de um pedaço, procura o próximo (até 5); o mesmo pedaço com os mesmos valores não volta.
- Caminho alternativo é outro pedaço: "clicar no item visível" só cabe com o item na tela; "abrir a busca e digitar"
  cabe sempre, e o Jev escolhe.
- Limites: o corte e a lacuna são heurísticos (um rótulo citado no objetivo, como o nome de um campo, também vira
  lacuna) e não se aprende o que nunca deu certo. Clique sem controle com nome embaixo não entra em pedaço. Segredo
  nunca vai para o arquivo.

### Progresso durante `objetivo`

Enquanto roda, cada etapa sai como `notifications/progress` (só se a chamada trouxer `_meta.progressToken`;
`progress` conta as etapas, sem `total`) e, se a chamada trouxer `_meta["claudecode/toolUseId"]`, também em
`~/.hangar/tool-progress/<toolUseId>.jsonl` (`{"t": <unix s>, "message": …}` por linha). `message` é um destes
rótulos fixos ou a descrição da ação executada (`invoke Button 'Enviar'`, `keys Enter`, `set_value Edit 'X' = ***`):

| `message` | fase |
|---|---|
| `conectando ao computador` / `reconectando ao computador` | conexão com o agente |
| `lendo a tela` | leitura da árvore de acessibilidade |
| `decidindo o próximo passo` | pergunta ao Jev (cerca de 0,5 s) |
| `capturando a tela` | print para o modelo de visão |
| `pedindo ajuda visual` | modelo de visão (o passo mais lento, 4 a 10 s) |
| qualquer outro texto | ação sendo executada |

Conclusão e falha não são notificação: vêm só no resultado da ferramenta (`concluído: …` / `parou: …`,
`isError` em falha interna). Sem notificação por muito tempo quer dizer etapa lenta (normalmente o modelo de
visão); quem chama decide quando avisar o usuário. A última notificação sempre sai antes do resultado.

## Quando ele para sozinho

- Ação arriscada (apagar, sobrescrever, enviar, fechar sem salvar) que o texto do objetivo não autorizou.
  Regra fixa, sem depender do Jev: o botão de confirmação de um diálogo (o que fica ao lado do Cancelar) e
  controle de apagar/excluir/remover/desconectar nunca entram nas opções se o objetivo não citar o diálogo, o
  botão ou o verbo; o Jev recebe a dica de cancelar o diálogo. Aviso só com Fechar/OK cobrindo a tela vira dica
  de fechá-lo antes.
  Repita o objetivo dizendo explicitamente ("…substituindo se já existir"). Fechar a janela (o botão Fechar
  no canto da janela, Alt+F4, Ctrl+Q, Ctrl+Shift+W) só acontece se o objetivo pedir para fechar.
- Navegador só é aberto quando o objetivo nomeia esse navegador ou diz "navegador"; um app cujo nome repete
  o de uma janela já aberta ("Chrome (CDP)" com o Chrome aberto) não é oferecido, salvo "nova"/"outra janela".
- A tela pede algo que não foi dado (login, credencial de rede): devolve isso como impedimento.
- O mesmo aviso do aplicativo volta duas vezes: devolve o texto do aviso.
- Sem progresso: 6 ciclos seguidos sem tela nova nesta execução (`parou: sem progresso: …`). Três ações
  seguidas sem efeito não param: o próximo passo passa pela ajuda visual pedindo outro caminho, e as mesmas
  ações não voltam. Com a árvore vazia o efeito é desconhecido (não conta como "sem efeito").
- `max_passos`/`limite_segundos`, só se quem chamou passar valores menores que os tetos.

## Limites conhecidos

- Texto de console (Windows Terminal) ainda não é lido: comando roda, mas o resultado não é conferido.
- Linux: o Chrome perde teclas digitadas em velocidade máxima (popup de preenchimento automático abrindo no meio
  da palavra), e o `wtype` perde `, { } > ' ( )` com teclado de teclas mortas (br ABNT2). `set_value` cola pela
  área de transferência (`wl-copy` + Ctrl+V) texto com mais de 40 caracteres ou com símbolos; texto curto é
  digitado, redigitado a 30 ms e só então colado. Sempre confere o campo depois. Colar substitui o conteúdo da
  área de transferência do usuário; senha nunca é colada. Antes de clicar, o agente confere que o cursor chegou
  ao ponto (`hyprctl cursorpos`) e falha em vez de clicar em outro lugar.
- Leitura da árvore em IDE Delphi leva 10 a 13 s por ciclo (VCL via MSAA); no resto, 1 s.
- Um objetivo por vez por Windows alvo (trava na pasta temporária, por usuário e por alvo).
- Linux, apps Chromium/Electron abertos sem leitor de tela (Slack, Discord): o agente pede os atributos do
  frame (`GetAttributes`), o mesmo sinal que o Orca dá, e o app liga a própria árvore sem reiniciar. Fica
  ligada até o app fechar (mais CPU no app). App aberto com acessibilidade desligada no barramento ou com
  `--disable-renderer-accessibility` não liga assim: segue só pelo print.

## Desenvolver

- Toolchain fixado em `rust-toolchain.toml`. Suíte local, sem Windows: `cargo test --workspace`.
- Testes num Windows por SSH, sem RDP: `HCC_WINDOWS_HOST=<alias-ssh> scripts/windows-test.sh <crate>...`.
- Binário: `cargo build --release` gera `target/release/hangar-computer-control`. Tag `v*` publicada
  compila Linux e Windows no GitHub Actions e anexa `hangar-computer-control-linux-x86_64` e
  `windows-agent.exe` à release.
- Detalhes do transporte remoto e empacotamento: `DESKTOP.md`.
