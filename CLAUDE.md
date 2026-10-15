# hangar-computer-control

Leia o `README.md` antes de mexer no laço (`crates/hcc-laco`).

## Entregar uma mudança

Trabalho pronto vai direto na `main` (sem PR). Toda entrega termina em release, senão ninguém recebe:

1. `cargo test --workspace` e `cargo clippy --workspace --all-targets` limpos.
2. Commit descritivo da mudança.
3. Subir a versão em `Cargo.toml` (`[workspace.package] version`): correção = patch, recurso = minor.
   `cargo build --release` atualiza o `Cargo.lock`; commit `chore: release X.Y.Z` só com os dois.
4. `git tag vX.Y.Z` e `git push origin main vX.Y.Z`. A tag dispara o GitHub Actions que publica
   `hangar-computer-control-linux-x86_64` e `windows-agent.exe` na release.
5. Instalar nesta máquina: copiar `target/release/hangar-computer-control` para
   `~/.hangar/computer-control/hangar-computer-control` (copiar para um temporário na mesma pasta e `mv`,
   o binário pode estar em uso). As sessões pegam a versão nova ao reiniciar o MCP.
