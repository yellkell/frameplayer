// Tells youtube-vr.js (in the page's world, which can't ask the extension)
// where FramePlayer's fonts are.
document.documentElement.dataset.frameFonts = chrome.runtime.getURL('fonts/');
