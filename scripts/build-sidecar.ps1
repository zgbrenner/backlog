<#
.SYNOPSIS
  Build the convertd Python sidecar into a single Windows executable for Tauri.

.DESCRIPTION
  Creates an isolated Python 3.11 venv (the ML deps have no 3.13/3.14 wheels),
  installs the pinned requirements, freezes the resolved set to
  sidecar/requirements.lock, runs PyInstaller, smoke-tests the binary, and copies
  it to src-tauri/binaries/ with the target-triple suffix Tauri expects.

  Python 3.11 is obtained via `uv` (userspace, no admin) by default. Pass
  -Python to use a specific interpreter instead.

.EXAMPLE
  pwsh scripts/build-sidecar.ps1
.EXAMPLE
  pwsh scripts/build-sidecar.ps1 -Clean -TargetTriple x86_64-pc-windows-msvc
#>
[CmdletBinding()]
param(
    [string]$Python = "",
    [string]$TargetTriple = "x86_64-pc-windows-msvc",
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$SidecarDir = Join-Path $RepoRoot "sidecar"
$VenvDir = Join-Path $SidecarDir ".venv-build"
$BinDir = Join-Path $RepoRoot "src-tauri/binaries"
$VenvPy = Join-Path $VenvDir "Scripts/python.exe"

Push-Location $SidecarDir
try {
    if ($Clean -and (Test-Path $VenvDir)) {
        Write-Host "Removing existing build venv..." -ForegroundColor Yellow
        Remove-Item -Recurse -Force $VenvDir
    }

    # 1. Create the Python 3.11 venv.
    if (-not (Test-Path $VenvPy)) {
        if ($Python) {
            Write-Host "Creating venv with $Python ..." -ForegroundColor Cyan
            & $Python -m venv $VenvDir
        }
        elseif (Get-Command uv -ErrorAction SilentlyContinue) {
            Write-Host "Creating venv with uv (standalone Python 3.11) ..." -ForegroundColor Cyan
            uv venv --python 3.11 $VenvDir
        }
        else {
            throw "No -Python given and 'uv' not found. Install uv (https://docs.astral.sh/uv/) or pass -Python C:\path\to\python3.11.exe"
        }
    }

    # Confirm the interpreter is 3.11 and 64-bit.
    $ver = & $VenvPy -c "import sys,struct; print(f'{sys.version_info.major}.{sys.version_info.minor}'); print(struct.calcsize('P')*8)"
    $pyver, $bits = $ver -split "`n" | ForEach-Object { $_.Trim() }
    if ($pyver -ne "3.11") { throw "Build venv is Python $pyver; the pinned deps require 3.11." }
    if ($bits -ne "64") { throw "Build venv is $bits-bit; a 64-bit interpreter is required." }

    # 2. Install deps. Prefer uv (fast) if present, else pip. Hash-checking mode
    #    (a lock with --hash) rejects unhashed args, so PyInstaller installs
    #    separately.
    $useUv = [bool](Get-Command uv -ErrorAction SilentlyContinue)
    $lock = Join-Path $SidecarDir "requirements.lock"
    Write-Host "Installing sidecar dependencies..." -ForegroundColor Cyan
    # $ErrorActionPreference = "Stop" does not apply to native commands, so every
    # install below is checked explicitly. A resolution failure here used to be
    # ignored: the run continued with only PyInstaller installed, --collect-all
    # logged twelve "not a package" warnings, and PyInstaller happily produced a
    # convertd.exe with no document parsers in it. The smoke test in step 4 caught
    # that one, but a build must not depend on a later gate to notice that its
    # dependencies were never installed.
    if (Test-Path $lock) {
        # requirements.lock is a fully-pinned (==) freeze for reproducibility.
        if ($useUv) { uv pip install --python $VenvPy -r $lock }
        else { & $VenvPy -m pip install -r $lock }
        if ($LASTEXITCODE -ne 0) { throw "Dependency install from $lock failed. The lock must resolve on Windows/Python 3.11 -- a lock produced on Linux can pin a set that is unsatisfiable here." }
    }
    else {
        Write-Warning "requirements.lock absent; installing from reviewed pins in requirements.txt and writing a fresh lock."
        if ($useUv) { uv pip install --python $VenvPy -r requirements.txt }
        else { & $VenvPy -m pip install -r requirements.txt }
        if ($LASTEXITCODE -ne 0) { throw "Dependency install from requirements.txt failed." }
        & $VenvPy -m pip freeze | Out-File -Encoding utf8 $lock
        Write-Host "Wrote reproducible lock: $lock" -ForegroundColor Green
    }
    if ($useUv) { uv pip install --python $VenvPy "pyinstaller>=6.11,<7" }
    else { & $VenvPy -m pip install "pyinstaller>=6.11,<7" }
    if ($LASTEXITCODE -ne 0) { throw "PyInstaller install failed." }

    # 3. Freeze convertd.py. --collect-all pulls the data files PyInstaller
    #    otherwise misses (model loaders, native libs, version metadata).
    #    Slim, torch-free profile: no torch/transformers/sentence_transformers/
    #    gliclass --collect-all entries. classify/salience/ettin_spans degrade
    #    to deterministic fallbacks at runtime instead (see convertd.py).
    #    onnxruntime/cv2/shapely/pyclipper are RapidOCR's native halves and
    #    ship .pyd/data files a bare --hidden-import does not pull; pdfminer
    #    (CMap tables) and pptx (its default template) are the same story for
    #    the markitdown parser extras.
    Write-Host "Running PyInstaller (slim, torch-free profile)..." -ForegroundColor Cyan
    & $VenvPy -m PyInstaller --clean --noconfirm --onefile --name convertd `
        --collect-all rapidocr `
        --collect-all lingua `
        --collect-all markitdown `
        --collect-all magika `
        --collect-all pypdfium2 `
        --collect-all onnxruntime `
        --collect-all cv2 `
        --collect-all shapely `
        --collect-all pyclipper `
        --collect-all pdfminer `
        --collect-all pptx `
        convertd.py
    if ($LASTEXITCODE -ne 0) { throw "PyInstaller failed." }

    $built = Join-Path $SidecarDir "dist/convertd.exe"
    if (-not (Test-Path $built)) { throw "Expected build output missing: $built" }

    # 4. Smoke test: drive real documents through one warm process.
    #    `ping` returns {} and every heavy component sits behind a lazy
    #    factory, so a ping-only gate passes a build with a missed hidden
    #    import or an uncollected ONNX data file and fails on the customer's
    #    first document. These three fixtures cover the three lanes:
    #    markitdown/mammoth (docx), markitdown/pdfminer (pdf), and
    #    rapidocr+onnxruntime (scanned png). All four requests go through ONE
    #    process, which is also how the app uses it.
    Write-Host "Smoke-testing the built sidecar on real fixtures..." -ForegroundColor Cyan

    # The request stream has to reach convertd as BOM-less UTF-8. PowerShell
    # encodes a native command's stdin with [Console]::InputEncoding -- whatever
    # the host console happens to be set to. When that is UTF-8 *with* a
    # preamble (a `chcp 65001` console, or any non-interactive host that sets
    # it), a 3-byte BOM is glued to the front of the first request, convertd
    # rejects it with `JSONDecodeError: Unexpected UTF-8 BOM`, its reply carries
    # `"id": null`, and this gate then reports "no response for request 1" --
    # blaming the binary for the harness's encoding. Pin it rather than inherit
    # it. (Restored in the outer finally.)
    $PreviousInputEncoding = [Console]::InputEncoding
    [Console]::InputEncoding = New-Object System.Text.UTF8Encoding $false

    $Fixtures = Join-Path $SidecarDir "fixtures"
    $requests = @(
        @{ id = 1; op = "versions" },
        @{ id = 2; op = "convert"; path = (Join-Path $Fixtures "sample_letter.docx"); head_pages = 10; tail_pages = 3 },
        @{ id = 3; op = "convert"; path = (Join-Path $Fixtures "sample_text.pdf"); head_pages = 10; tail_pages = 3 },
        @{ id = 4; op = "ocr"; path = (Join-Path $Fixtures "sample_scan.png"); dpi = 300; head_pages = 10; tail_pages = 3 }
    ) | ForEach-Object { $_ | ConvertTo-Json -Compress }

    $responses = @{}
    $requests | & $built | ForEach-Object {
        if ($_.Trim()) {
            $parsed = $_ | ConvertFrom-Json
            $responses[[int]$parsed.id] = $parsed
        }
    }

    foreach ($id in 1..4) {
        if (-not $responses.ContainsKey($id)) { throw "Smoke test failed; no response for request $id." }
        if ($responses[$id].ok -ne $true) {
            throw "Smoke test failed; request ${id}: $($responses[$id].error)"
        }
    }
    if (-not $responses[1].convertd) { throw "Smoke test failed; 'versions' reported no convertd version." }
    foreach ($id in 2, 3) {
        if ([string]::IsNullOrWhiteSpace($responses[$id].markdown)) {
            throw "Smoke test failed; request $id converted to empty markdown -- a parser dependency is missing from the frozen binary."
        }
    }
    # OCR is graded on the lane having run, not on recognition quality: an
    # uncollected ONNX model or missing cv2 makes RapidOCR raise on
    # construction, which shows up as ok=false above.
    if ($responses[4].ocr_used -ne $true -or [int]$responses[4].page_count -lt 1) {
        throw "Smoke test failed; the OCR lane did not run on sample_scan.png."
    }
    Write-Host "Smoke test passed: versions + docx/pdf convert + png OCR." -ForegroundColor Green

    # 5. Place it where Tauri's externalBin expects it.
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $dest = Join-Path $BinDir "convertd-$TargetTriple.exe"
    Copy-Item -Force $built $dest
    $sha = (Get-FileHash -Algorithm SHA256 $dest).Hash.ToLower()
    Write-Host ""
    Write-Host "convertd sidecar built:" -ForegroundColor Green
    Write-Host "  $dest"
    Write-Host "  SHA-256: $sha"
    Write-Host "  Now stage llama-server (see RELEASING.md step 2) and run 'npm run tauri build'."
}
finally {
    if ($null -ne $PreviousInputEncoding) { [Console]::InputEncoding = $PreviousInputEncoding }
    Pop-Location
}
