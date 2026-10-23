use serde::{Deserialize, Serialize};

/// 引擎状态
#[derive(Debug, Serialize, Deserialize)]
pub struct EngineStatus {
    pub cloud_ready: bool,
    pub tts_ready: bool,
}

/// 图像描述请求
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageDescriptionRequest {
    pub image_data: Vec<u8>,
    pub format: Option<String>,
}

/// 图像描述响应
#[derive(Debug, Serialize, Deserialize)]
pub struct ImageDescriptionResponse {
    pub description: String,
    pub confidence: Option<f32>,
}

/// 语音识别请求
#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptionRequest {
    pub audio_data: Vec<u8>,
    pub format: Option<String>,
    pub language: Option<String>,
}

/// 语音识别响应
#[derive(Debug, Serialize, Deserialize)]
pub struct TranscriptionResponse {
    pub text: String,
    pub confidence: Option<f32>,
    pub language: Option<String>,
}

/// 语音合成请求
#[derive(Debug, Serialize, Deserialize)]
pub struct SynthesisRequest {
    pub text: String,
    pub voice: Option<String>,
    pub speed: Option<f32>,
}

/// 语音合成响应
#[derive(Debug, Serialize, Deserialize)]
pub struct SynthesisResponse {
    pub audio_data: Vec<u8>,
    pub format: String,
    pub duration: Option<f32>,
}

/// 模型信息
#[derive(Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub version: String,
    pub device: String,
    pub loaded: bool,
}

/// 系统状态
#[derive(Debug, Serialize, Deserialize)]
pub struct SystemStatus {
    pub engine: EngineStatus,
    pub models: Vec<ModelInfo>,
    pub platform: String,
    pub version: String,
}
