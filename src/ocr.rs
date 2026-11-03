//! Text recognition: Vision on macOS, Windows.Media.Ocr on Windows.

use anyhow::Result;
use image::RgbaImage;

#[cfg(target_os = "macos")]
pub fn recognize(img: &RgbaImage) -> Result<String> {
    use anyhow::anyhow;
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("rega-ocr-{}-{nanos}.png", std::process::id()));
    img.save(&tmp)?;

    let result = (|| -> Result<String> {
        let path = NSString::from_str(&tmp.to_string_lossy());
        let url = NSURL::fileURLWithPath(&path);
        let options = NSDictionary::new();
        // SAFETY: plain Vision API calls with valid, retained arguments.
        let handler = unsafe {
            VNImageRequestHandler::initWithURL_options(
                VNImageRequestHandler::alloc(),
                &url,
                &options,
            )
        };
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        let as_request: &VNRequest = &request;
        let requests = NSArray::from_slice(&[as_request]);
        handler
            .performRequests_error(&requests)
            .map_err(|e| anyhow!("Vision failed: {}", e.localizedDescription()))?;

        let mut lines = Vec::new();
        if let Some(results) = request.results() {
            for observation in results.iter() {
                if let Some(candidate) = observation.topCandidates(1).firstObject() {
                    lines.push(candidate.string().to_string());
                }
            }
        }
        Ok(lines.join("\n"))
    })();

    let _ = std::fs::remove_file(&tmp);
    result
}

/// Windows: the built-in Windows.Media.Ocr engine, driven through PowerShell.
#[cfg(target_os = "windows")]
pub fn recognize(img: &RgbaImage) -> Result<String> {
    use std::os::windows::process::CommandExt;
    const NO_WINDOW: u32 = 0x0800_0000;
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]
$null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Foundation, ContentType = WindowsRuntime]
$null = [Windows.Storage.StorageFile, Windows.Foundation, ContentType = WindowsRuntime]
$asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
function Await($op, $type) { $t = $asTask.MakeGenericMethod($type).Invoke($null, @($op)); $t.Wait(-1) | Out-Null; $t.Result }
$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($env:REGA_OCR_IMAGE)) ([Windows.Storage.StorageFile])
$stream = Await ($file.OpenReadAsync()) ([Windows.Storage.Streams.IRandomAccessStreamWithContentType])
$decoder = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder])
$bitmap = Await ($decoder.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap])
$engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages()
if (-not $engine) { throw 'No OCR language pack is installed' }
$result = Await ($engine.RecognizeAsync($bitmap)) ([Windows.Media.Ocr.OcrResult])
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
($result.Lines | ForEach-Object { $_.Text }) -join "`n"
"#;
    let tmp = temp_png(img)?;
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("REGA_OCR_IMAGE", &tmp)
        .creation_flags(NO_WINDOW)
        .output();
    let _ = std::fs::remove_file(&tmp);
    let output = output?;
    anyhow::ensure!(output.status.success(), "Windows OCR failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(target_os = "windows")]
fn temp_png(img: &RgbaImage) -> Result<std::path::PathBuf> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("rega-ocr-{}-{nanos}.png", std::process::id()));
    img.save(&tmp)?;
    Ok(tmp)
}
