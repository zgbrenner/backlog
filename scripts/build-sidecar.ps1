<#
.SYNOPSIS
  Build the convertd Python sidecar into a single Windows executable for Tauri.

.DESCRIPTION
  Creates an isolated Python 3.11 venv (the ML deps have no 3.13/3.14 wheels),
  installs the pinned requirements, freezes the resolved set to
  sidecar/requirements.lock, runs PyInstaller in --onedir mode, smoke-tests the
  built convertd.exe, and stages the whole output directory (convertd.exe plus
  its _internal/ tree of DLLs and data files) to src-tauri/binaries/convertd/.

  --onedir rather than --onefile: a --onefile exe re-extracts its entire ~250 MB
  bundle to a fresh %TEMP%\_MEI* folder on EVERY launch, which a
  virus-scanned corporate machine turns into a 34-52s startup and ~250 MB of
  temp garbage per run. --onedir extracts nothing at runtime, so convertd
  starts in ~1-2s. The directory output can no longer travel through Tauri's
  externalBin (which only stages a single file); it ships through
  bundle.resources instead -- see tauri.conf.json and src-tauri/src/lib.rs.

  Python 3.11 is obtained via `uv` (userspace, no admin) by default. Pass
  -Python to use a specific interpreter instead.

.EXAMPLE
  pwsh scripts/build-sidecar.ps1
.EXAMPLE
  pwsh scripts/build-sidecar.ps1 -Clean
#>
[CmdletBinding()]
param(
    [string]$Python = "",
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$SidecarDir = Join-Path $RepoRoot "sidecar"
$VenvDir = Join-Path $SidecarDir ".venv-build"
$BinDir = Join-Path $RepoRoot "src-tauri/binaries"
$VenvPy = Join-Path $VenvDir "Scripts/python.exe"
$ReleaseModelDir = Join-Path $RepoRoot "src-tauri/resources/models"
$SemanticModel = Join-Path $ReleaseModelDir "semantic\all-MiniLM-L6-v2\model.onnx"
$SemanticVocab = Join-Path $ReleaseModelDir "semantic\all-MiniLM-L6-v2\vocab.txt"
$SemanticModelSha256 = "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1"
$SemanticVocabSha256 = "07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3"

function Assert-Hash {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Expected,
        [Parameter(Mandatory)][string] $Label
    )
    if (-not (Test-Path $Path)) {
        throw "$Label is missing: $Path. Run scripts/stage-release-inputs.ps1 first."
    }
    $actual = (Get-FileHash $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Expected) {
        throw "$Label hash mismatch: expected $Expected, computed $actual"
    }
}

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

    # 2. Install runtime and build dependencies from separate exact locks.
    #    Keeping PyInstaller and its helpers out of the shipped runtime lock
    #    avoids bundling build tools while still making the freezer repeatable.
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
    $buildLock = Join-Path $SidecarDir "build-requirements.lock"
    if (-not (Test-Path $buildLock)) {
        throw "Build dependency lock is missing: $buildLock"
    }
    if ($useUv) { uv pip install --python $VenvPy -r $buildLock }
    else { & $VenvPy -m pip install -r $buildLock }
    if ($LASTEXITCODE -ne 0) {
        throw "Build dependency install from $buildLock failed."
    }

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
    & $VenvPy -m PyInstaller --clean --noconfirm --onedir --name convertd `
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

    # --onedir output is a directory: dist/convertd/convertd.exe plus its
    # _internal/ tree of DLLs and data files. Both must survive staging intact
    # (see step 5) or convertd fails to start on the target machine.
    $builtDir = Join-Path $SidecarDir "dist/convertd"
    $built = Join-Path $builtDir "convertd.exe"
    if (-not (Test-Path $built)) { throw "Expected build output missing: $built" }
    $builtInternal = Join-Path $builtDir "_internal"
    if (-not (Test-Path $builtInternal)) { throw "Expected build output missing: $builtInternal" }

    # 4. Smoke test: drive real documents and real semantic ops through one warm process.
    #    `ping` returns {} and every heavy component sits behind a lazy
    #    factory, so a ping-only gate passes a build with a missed hidden
    #    import or an uncollected ONNX data file and fails on the customer's
    #    first document. These three fixtures cover the three lanes:
    #    markitdown/mammoth (docx), markitdown/pdfminer (pdf), and
    #    rapidocr+onnxruntime (scanned png), plus the ONNX MiniLM semantic
    #    ranker and cached-label entity extractor. All six requests go
    #    through ONE process, which is also how the app uses it.
    Write-Host "Smoke-testing the built sidecar on real fixtures..." -ForegroundColor Cyan
    Assert-Hash $SemanticModel $SemanticModelSha256 "semantic\all-MiniLM-L6-v2\model.onnx"
    Assert-Hash $SemanticVocab $SemanticVocabSha256 "semantic\all-MiniLM-L6-v2\vocab.txt"

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
    $PreviousModelsDir = $env:BACKLOG_MODELS_DIR
    $env:BACKLOG_MODELS_DIR = $ReleaseModelDir

    $Fixtures = Join-Path $SidecarDir "fixtures"
    $SemanticParagraphs = @(
        @{
            index = 0
            text = "Jane Doe's employment terminates effective July 31, 2026 under the Acme Corporation agreement."
            start_char = 0
            end_char = 94
        },
        @{
            index = 1
            text = "The warehouse door was painted blue during routine maintenance."
            start_char = 95
            end_char = 158
        }
    )
    $requests = @(
        @{ id = 1; op = "versions" },
        @{ id = 2; op = "convert"; path = (Join-Path $Fixtures "sample_letter.docx"); head_pages = 10; tail_pages = 3 },
        @{ id = 3; op = "convert"; path = (Join-Path $Fixtures "sample_text.pdf"); head_pages = 10; tail_pages = 3 },
        @{ id = 4; op = "ocr"; path = (Join-Path $Fixtures "sample_scan.png"); dpi = 300; head_pages = 10; tail_pages = 3 },
        @{ id = 5; "op" = "rank_paragraphs"; paragraphs = $SemanticParagraphs; probes = @("employment termination date"); top_k = 1; min_score = 0.0 },
        @{ id = 6; "op" = "extract_entities"; paragraphs = $SemanticParagraphs; labels = @(@{ label = "PERSON"; description = "a human person's full name" }, @{ label = "ORGANIZATION"; description = "a company or organization" }); threshold = 0.0 }
    ) | ForEach-Object { $_ | ConvertTo-Json -Compress }

    $responses = @{}
    $requests | & $built | ForEach-Object {
        if ($_.Trim()) {
            $parsed = $_ | ConvertFrom-Json
            $responses[[int]$parsed.id] = $parsed
        }
    }

    foreach ($id in 1..6) {
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
    foreach ($id in 5, 6) {
        if ($responses[$id].available -ne $true) {
            throw "Smoke test failed; semantic request $id did not report available: true."
        }
    }
    if ($responses[5].results.Count -lt 1) {
        throw "Smoke test failed; semantic ranker returned no ranked paragraphs."
    }
    Write-Host "Smoke test passed: versions + docx/pdf convert + png OCR + semantic ranking/entities." -ForegroundColor Green

    # 5. Place it where Tauri's bundle.resources expects it. convertd is no
    #    longer an externalBin single file, so there is no target-triple
    #    suffix here -- tauri.conf.json stages the whole directory verbatim
    #    via "binaries/convertd/": "convertd/". Clean the staging dir first so
    #    a file removed from a later PyInstaller run (e.g. a dropped
    #    --collect-all) doesn't linger from a previous build and mask a
    #    regression.
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $destDir = Join-Path $BinDir "convertd"
    if (Test-Path $destDir) {
        Remove-Item -Recurse -Force $destDir
    }
    Copy-Item -Recurse -Force $builtDir $destDir
    $destExe = Join-Path $destDir "convertd.exe"
    $sha = (Get-FileHash -Algorithm SHA256 $destExe).Hash.ToLower()
    Write-Host ""
    Write-Host "convertd sidecar built:" -ForegroundColor Green
    Write-Host "  $destDir"
    Write-Host "  convertd.exe SHA-256: $sha"
    Write-Host "  Now stage llama-server (see RELEASING.md step 2) and run 'npm run tauri build'."
}
finally {
    if ($null -ne $PreviousModelsDir) { $env:BACKLOG_MODELS_DIR = $PreviousModelsDir }
    else { Remove-Item Env:BACKLOG_MODELS_DIR -ErrorAction SilentlyContinue }
    if ($null -ne $PreviousInputEncoding) { [Console]::InputEncoding = $PreviousInputEncoding }
    Pop-Location
}
