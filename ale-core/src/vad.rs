use std::collections::VecDeque;

/// VAD 检测结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadState {
    /// 静默
    Silent,
    /// 正在说话
    Speaking,
    /// 说话刚结束（触发处理的瞬间）
    SpeechEnded,
}

/// VAD 配置
#[derive(Debug, Clone)]
pub struct VadConfig {
    /// 能量阈值（0.0-1.0），低于此值视为静默
    pub energy_threshold: f32,
    /// 语音开始所需的连续语音帧数
    pub speech_start_frames: usize,
    /// 语音结束所需的连续静默帧数
    pub silence_end_frames: usize,
    /// 采样率
    pub sample_rate: u32,
    /// 每帧采样数（10ms/20ms/30ms）
    pub frame_size: usize,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            energy_threshold: 0.015,
            speech_start_frames: 3,
            silence_end_frames: 25, // ~500ms at 20ms frames — 给呼吸暂停留出空间
            sample_rate: 16000,
            frame_size: 320, // 20ms at 16kHz
        }
    }
}

impl VadConfig {
    /// 创建针对弱语音优化的配置（呼吸衰竭/气短患者）
    pub fn weak_voice() -> Self {
        Self {
            energy_threshold: 0.008,
            speech_start_frames: 2,
            silence_end_frames: 35, // ~700ms — 更长的呼吸暂停容忍
            sample_rate: 16000,
            frame_size: 320,
        }
    }
}

/// 语音活动检测器（基于能量的简易实现）
pub struct VoiceActivityDetector {
    pub config: VadConfig,
    state: VadState,
    speech_frame_count: usize,
    silence_frame_count: usize,
    energy_history: VecDeque<f32>,
    max_history: usize,
}

impl VoiceActivityDetector {
    pub fn new(config: VadConfig) -> Self {
        let max_history = 50;
        Self {
            config,
            state: VadState::Silent,
            speech_frame_count: 0,
            silence_frame_count: 0,
            energy_history: VecDeque::with_capacity(max_history),
            max_history,
        }
    }

    pub fn with_default_config() -> Self {
        Self::new(VadConfig::default())
    }

    /// 获取当前状态
    pub fn state(&self) -> VadState {
        self.state
    }

    /// 重置状态
    pub fn reset(&mut self) {
        self.state = VadState::Silent;
        self.speech_frame_count = 0;
        self.silence_frame_count = 0;
    }

    /// 处理一帧音频数据，返回更新后的状态
    /// `samples` 应为 f32 格式，范围 [-1.0, 1.0]
    pub fn process_frame(&mut self, samples: &[f32]) -> VadState {
        let energy = self.compute_rms_energy(samples);

        // 更新能量历史
        self.energy_history.push_back(energy);
        if self.energy_history.len() > self.max_history {
            self.energy_history.pop_front();
        }

        let is_speech = energy > self.config.energy_threshold;

        match self.state {
            VadState::Silent => {
                if is_speech {
                    self.speech_frame_count += 1;
                    self.silence_frame_count = 0;
                    if self.speech_frame_count >= self.config.speech_start_frames {
                        self.state = VadState::Speaking;
                    }
                } else {
                    self.speech_frame_count = 0;
                }
            }
            VadState::Speaking => {
                if is_speech {
                    self.silence_frame_count = 0;
                } else {
                    self.silence_frame_count += 1;
                    if self.silence_frame_count >= self.config.silence_end_frames {
                        self.state = VadState::SpeechEnded;
                    }
                }
            }
            VadState::SpeechEnded => {
                // SpeechEnded 是瞬态，下一帧自动回到 Silent
                self.state = VadState::Silent;
                self.speech_frame_count = 0;
                self.silence_frame_count = 0;
            }
        }

        self.state
    }

    /// 计算 RMS 能量
    fn compute_rms_energy(&self, samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_squares: f32 = samples.iter().map(|s| s * s).sum();
        (sum_squares / samples.len() as f32).sqrt()
    }

    /// 获取平均能量（用于调试/自适应阈值）
    pub fn average_energy(&self) -> f32 {
        if self.energy_history.is_empty() {
            return 0.0;
        }
        self.energy_history.iter().sum::<f32>() / self.energy_history.len() as f32
    }

    /// 自适应调整阈值（基于历史能量）
    pub fn adapt_threshold(&mut self) {
        let avg = self.average_energy();
        if avg > 0.0 {
            // 设置阈值为平均能量的 1.5 倍
            self.config.energy_threshold = (avg * 1.5).clamp(0.005, 0.5);
        }
    }
}

/// 将音频数据按帧分割并处理
pub fn process_audio_chunks(
    vad: &mut VoiceActivityDetector,
    audio_data: &[f32],
) -> Vec<(usize, VadState)> {
    let frame_size = vad.config.frame_size;
    let mut results = Vec::new();

    for (i, chunk) in audio_data.chunks(frame_size).enumerate() {
        if chunk.len() == frame_size {
            let state = vad.process_frame(chunk);
            results.push((i * frame_size, state));
        }
    }

    results
}

/// 从 i16 PCM 转换为 f32
pub fn i16_to_f32(data: &[i16]) -> Vec<f32> {
    data.iter().map(|&s| s as f32 / 32768.0).collect()
}

/// 从字节 PCM16 转换为 f32
pub fn pcm16_bytes_to_f32(data: &[u8]) -> Vec<f32> {
    data.chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / 32768.0
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_silence_detection() {
        let mut vad = VoiceActivityDetector::with_default_config();
        let silence = vec![0.0f32; 320];

        for _ in 0..10 {
            let state = vad.process_frame(&silence);
            assert_eq!(state, VadState::Silent);
        }
    }

    #[test]
    fn test_speech_detection() {
        let mut vad = VoiceActivityDetector::with_default_config();
        let silence = vec![0.0f32; 320];
        let speech: Vec<f32> = (0..320).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();

        // 先静默几帧
        for _ in 0..5 {
            vad.process_frame(&silence);
        }

        // 开始说话
        let mut last_state = VadState::Silent;
        for _ in 0..10 {
            last_state = vad.process_frame(&speech);
        }
        assert_eq!(last_state, VadState::Speaking);
    }

    #[test]
    fn test_speech_ended_detection() {
        let mut vad = VoiceActivityDetector::with_default_config();
        let silence = vec![0.0f32; 320];
        let speech: Vec<f32> = (0..320).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();

        // 进入说话状态
        for _ in 0..5 {
            vad.process_frame(&silence);
        }
        for _ in 0..10 {
            vad.process_frame(&speech);
        }
        assert_eq!(vad.state(), VadState::Speaking);

        // 开始静默，前24帧仍然处于 Speaking
        for _ in 0..24 {
            let state = vad.process_frame(&silence);
            assert_eq!(state, VadState::Speaking);
        }

        // 第25帧静默 -> SpeechEnded
        let state = vad.process_frame(&silence);
        assert_eq!(state, VadState::SpeechEnded);

        // 下一帧自动回到 Silent
        let state = vad.process_frame(&silence);
        assert_eq!(state, VadState::Silent);
    }

    #[test]
    fn test_rms_energy() {
        let vad = VoiceActivityDetector::with_default_config();

        let silence = vec![0.0f32; 320];
        assert!((vad.compute_rms_energy(&silence) - 0.0).abs() < 1e-6);

        let loud = vec![1.0f32; 320];
        assert!((vad.compute_rms_energy(&loud) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_process_audio_chunks() {
        let mut vad = VoiceActivityDetector::with_default_config();
        let audio = vec![0.0f32; 3200]; // 100ms at 16kHz

        let results = process_audio_chunks(&mut vad, &audio);
        assert_eq!(results.len(), 10);
    }

    #[test]
    fn test_adapt_threshold() {
        let mut vad = VoiceActivityDetector::with_default_config();
        let low_noise: Vec<f32> = (0..320).map(|i| (i as f32 * 0.01).sin() * 0.001).collect();

        for _ in 0..50 {
            vad.process_frame(&low_noise);
        }
        vad.adapt_threshold();
        assert!(vad.config.energy_threshold > 0.0);
    }
}
