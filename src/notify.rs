//! 提示音：Windows 下用 PlaySound 从内存播放内嵌 wav（异步、无需外部文件）。

const WAV: &[u8] = include_bytes!("../assets/notify.wav");

#[cfg(windows)]
pub fn play() {
    use windows_sys::Win32::Media::Audio::{PlaySoundA, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    unsafe {
        PlaySoundA(
            WAV.as_ptr() as _,
            std::ptr::null_mut(),
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

#[cfg(not(windows))]
pub fn play() {
    // 其他平台暂不发声（程序本身面向 Windows）
    let _ = WAV;
}
