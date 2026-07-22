//! §3: ingest routing. Type by magic bytes (never extension); the PDF
//! text-layer probe itself runs in the sidecar (pypdfium2), this module just
//! decides what to do with the answer.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Native,        // MarkItDown path
    Scanned,       // raster + OCR path
    Flag,          // unprocessable
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteDecision {
    pub route: Route,
    pub detected_type: String,
    pub flag_reason: Option<String>,
}

const NATIVE_TYPES: &[&str] = &[
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/msword",
    "application/vnd.ms-powerpoint",
    "application/vnd.ms-excel",
    "application/rtf",
    "text/html",
    "text/plain",
    "text/csv",
    "message/rfc822",
];

const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/tiff", "image/bmp", "image/webp"];

pub fn detect(path: &Path) -> RouteDecision {
    let bytes = match std::fs::read(path) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => {
            return RouteDecision {
                route: Route::Flag,
                detected_type: "empty".into(),
                flag_reason: Some("CORRUPT:zero-byte file".into()),
            }
        }
        Err(e) => {
            return RouteDecision {
                route: Route::Flag,
                detected_type: "unreadable".into(),
                flag_reason: Some(format!("CORRUPT:read error {e}")),
            }
        }
    };

    let kind = infer::get(&bytes);
    let mime = kind.map(|k| k.mime_type().to_string()).unwrap_or_else(|| {
        // infer misses plain text and eml; sniff cheaply.
        if bytes.iter().take(2048).all(|b| *b == 9 || *b == 10 || *b == 13 || *b >= 32) {
            "text/plain".to_string()
        } else {
            "application/octet-stream".to_string()
        }
    });

    if mime == "application/pdf" {
        // Text-layer test happens in the sidecar; caller re-routes on the answer.
        return RouteDecision { route: Route::Native, detected_type: mime, flag_reason: None };
    }
    if IMAGE_TYPES.contains(&mime.as_str()) {
        return RouteDecision { route: Route::Scanned, detected_type: mime, flag_reason: None };
    }
    if NATIVE_TYPES.iter().any(|t| mime.starts_with(t)) || mime.starts_with("text/") {
        return RouteDecision { route: Route::Native, detected_type: mime, flag_reason: None };
    }
    // Legacy zip-based office files sometimes sniff as zip.
    if mime == "application/zip" {
        return RouteDecision { route: Route::Native, detected_type: mime, flag_reason: None };
    }

    RouteDecision {
        route: Route::Flag,
        detected_type: mime.clone(),
        flag_reason: Some(format!("UNSUPPORTED_TYPE:{mime}")),
    }
}

pub fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "bin".into())
}
