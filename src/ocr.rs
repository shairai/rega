//! Text recognition. macOS uses the built-in Vision framework; other
//! platforms report that OCR is not available yet.

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
    let tmp = std::env::temp_dir().join(format!("snapquill-ocr-{}-{nanos}.png", std::process::id()));
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

#[cfg(not(target_os = "macos"))]
pub fn recognize(_img: &RgbaImage) -> Result<String> {
    anyhow::bail!("Text recognition is only available on macOS in this version")
}
