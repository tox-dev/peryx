$ErrorActionPreference = "Stop"
if ((sccache --version) -ne "sccache 0.16.0") {
    throw "unexpected sccache version"
}
$root = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$directory = Join-Path $root "sccache-response-file"
New-Item -ItemType Directory -Force $directory | Out-Null
$source = Join-Path $directory "response_file.rs"
Set-Content -Encoding utf8NoBOM -Path $source -Value "pub fn response_file_works() {}"
$response = Join-Path $directory "arguments"
@(
    "--crate-name=sccache_response_file"
    "--crate-type=rlib"
    "--emit=link,dep-info"
    "--out-dir=$directory"
    1..2048 | ForEach-Object { "--cfg"; "sccache_response_file_$($_)" }
    $source
) | Set-Content -Encoding utf8NoBOM -Path $response
sccache rustc "@$response"
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
if (-not (Test-Path (Join-Path $directory "libsccache_response_file.rlib"))) {
    throw "rustc did not produce the response-file artifact"
}
