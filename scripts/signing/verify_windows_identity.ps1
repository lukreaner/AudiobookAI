$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$thumbprint = $env:WINDOWS_CERTIFICATE_THUMBPRINT
if ([string]::IsNullOrWhiteSpace($thumbprint)) {
    throw "Windows signing setup failed: local certificate thumbprint is not configured"
}

$normalized = $thumbprint.Replace(" ", "").ToUpperInvariant()
$certificate = Get-ChildItem -Path "Cert:\CurrentUser\My" | Where-Object {
    $_.Thumbprint.ToUpperInvariant() -eq $normalized
} | Select-Object -First 1

if ($null -eq $certificate) {
    throw "Windows signing setup failed: configured certificate is absent from CurrentUser/My"
}
if (-not $certificate.HasPrivateKey) {
    throw "Windows signing setup failed: configured certificate has no locally accessible private key"
}
if ($certificate.NotAfter -le (Get-Date)) {
    throw "Windows signing setup failed: configured certificate is expired"
}

$codeSigningOid = "1.3.6.1.5.5.7.3.3"
$ekuExtension = $certificate.Extensions | Where-Object {
    $_.Oid.Value -eq "2.5.29.37"
} | Select-Object -First 1
$supportsCodeSigning = $null -ne $ekuExtension -and $null -ne ($ekuExtension.EnhancedKeyUsages | Where-Object {
    $_.Value -eq $codeSigningOid
} | Select-Object -First 1)
if (-not $supportsCodeSigning) {
    throw "Windows signing setup failed: configured certificate is not valid for code signing"
}

Write-Host "Local Windows signing identity is available"
