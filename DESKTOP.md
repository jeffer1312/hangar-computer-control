# Controle por acessibilidade do Windows

O agente `windows-agent.exe` roda na sessão gráfica do usuário. O MCP fica junto ao
cliente (Codex/Claude/Hangar) e usa `HCC_AGENT_CONFIG` (obrigatória) para escolher
o agente local ou remoto.

O agente não tem Jev nem LLM: fornece janelas, controles e ações pela UI Automation.
O laço (`laco_uia.py`) monta as ações possíveis a partir da árvore e o Jev escolhe uma
por ciclo. O LLM (`LLM_PROXY_URL`, `LLM_PROXY_KEY`, `LLM_MODEL`) só entra quando o Jev
não acha saída ou a árvore vem vazia: recebe a imagem, interpreta e propõe ações que
voltam para o Jev escolher. Ação com risco (apagar, sobrescrever, enviar) não pedida
no objetivo para e volta para o chamador.
Referências valem para uma observação e uma ação. Mudança de foco, expiração e
desconexão precisam de nova observação; captura antiga não é resposta válida.

## Empacotar no Windows

Execute `scripts/build-windows-agent.ps1`. O resultado é `dist/windows-agent.exe`,
incluindo Python e as bibliotecas. A máquina de destino não precisa instalar Python.
O MCP controlador continua precisando do ambiente Python com suas dependências.
O executável ainda não possui assinatura digital.

## Agente na mesma máquina

Crie um arquivo JSON apontando para o executável:

```json
{"transport":"local","command":["C:\\HangarComputerControl\\windows-agent.exe"]}
```

O MCP inicia o processo na sessão em que está rodando, gera a credencial de retorno
e encerra o processo ao fechar a conexão. Não há código de pareamento manual.
É necessário usuário logado no desktop; não iniciar como serviço na sessão 0.

## Agente remoto

`exemplo-agent.json` é o modelo. Uma configuração usa `transport: ssh`, `host` e `agent_path` (executável disponível no
controlador). `proxy_command` é opcional. `shared_executable` é um caminho visível
no Windows para copiar pelo compartilhamento; sem ele, o envio é feito por SCP.

Requisitos: SSH previamente autorizado, usuário com sessão gráfica ativa e permissão
para criar sua tarefa interativa. A conexão SSH abre um túnel temporário de loopback.
O agente é copiado para uma pasta identificada pelo SHA-256, conferido e iniciado por
uma tarefa temporária. A credencial é enviada pelo canal SSH, sem reaproveitar o token
do Hangar. Ao encerrar, o controlador remove sua tarefa e fecha o túnel; o binário fica
para reutilização. Não altera serviços nem abre porta pública.

Isso não instala SSH nem configura RDP automaticamente em uma máquina desconhecida.
Tela bloqueada/UAC e aplicativos que não expõem controles podem impedir uma ação.

## Usar no Hangar

O Hangar já inicia os MCPs configurados no harness; não precisa carregar a automação
no seu backend. O registro abaixo cria o MCP separado `desktop`, o servidor principal é `hangar-computer-control`:

```bash
.venv/bin/python scripts/register_desktop_mcp.py --target codex \
  --client-config /caminho/da/conta/config.toml \
  --agent-config /caminho/para/config-do-agente.json --check
```

Remova `--check` para gravar, com cópia da configuração anterior. Para Claude, use
`--target claude` e o arquivo de configuração MCP correspondente. O registro não
sobrescreve um MCP `desktop` preexistente de outra origem. `TYPESAFE_API_KEY` deve
estar no ambiente da sessão; no Hangar, isso corresponde à opção Jev do navegador.
As chaves do LLM/Jev permanecem no controlador, não são enviadas ao agente Windows.

Novas sessões carregam o registro; processos MCP já abertos não recarregam Python
automaticamente. Esta integração não altera nem publica os instaladores do Hangar.

## Verificação

`python -m unittest -v` executa os testes locais. O teste com modelos e Windows reais
deve usar um processo MCP novo e registrar os tempos e a captura final. Simulações
de contrato não comprovam autonomia nem comportamento em telas Delphi.
