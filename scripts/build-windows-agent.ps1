param([string]$Source = (Split-Path $PSScriptRoot), [string]$Output)
$ErrorActionPreference = 'Stop'
if (-not $Output) { $Output = Join-Path $Source 'dist' }
$build = Join-Path $env:LOCALAPPDATA 'HangarComputerControlBuild'
New-Item -ItemType Directory -Force $build | Out-Null
if (-not (Test-Path "$build\venv\Scripts\python.exe")) {
    python -m venv "$build\venv"
    if ($LASTEXITCODE -ne 0) { throw 'Falha ao criar ambiente de build' }
}
$python = "$build\venv\Scripts\python.exe"
& $python -m pip install pywinauto==0.6.9 Pillow==11.3.0 pyinstaller==6.16.0
if ($LASTEXITCODE -ne 0) { throw 'Falha ao instalar dependências de build' }
# Gere os wrappers COM antes de empacotar; não há Python instalado no destino final.
& $python -c 'import pywinauto'
if ($LASTEXITCODE -ne 0) { throw 'Falha ao carregar UI Automation' }
& $python -m PyInstaller --noconfirm --clean --onefile --noconsole --name windows-agent `
    --collect-submodules comtypes.gen --collect-submodules pywinauto `
    --distpath $Output --workpath "$build\work" --specpath $build "$Source\windows_uia.py"
if ($LASTEXITCODE -ne 0) { throw 'Falha ao gerar executável' }
Get-FileHash "$Output\windows-agent.exe" -Algorithm SHA256
