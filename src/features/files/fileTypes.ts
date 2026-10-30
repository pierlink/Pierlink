/**
 * 一看扩展名就知道不是文本的文件：图片、音视频、压缩包、文档、可执行文件、字体、数据/模型文件等。
 * 这些文件不在软件里编辑（打开大图片之类会很卡）。其他扩展名（含没有扩展名的）都允许尝试，
 * 读的时候核心库再按内容判断（有空字符或不是 UTF-8 就拒绝）。SVG 是文本，不在列表里。
 * 有歧义的扩展名不放进来，交给内容判断：.ts（多数是 TypeScript，少数是视频流）、.obj（3D 模型是文本，编译产物是二进制）。
 */
const BINARY = new Set([
  // 图片
  "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "icns", "tif", "tiff", "psd", "heic", "heif", "avif", "raw", "cr2", "nef", "dng", "xcf",
  // 音视频
  "mp3", "wav", "flac", "ogg", "m4a", "aac", "wma", "opus", "mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "3gp",
  // 压缩包、镜像、安装包
  "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "lzma", "jar", "war", "whl", "deb", "rpm", "apk", "dmg", "iso", "img", "msi", "cab", "appimage",
  // 文档
  "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "epub", "mobi",
  // 可执行文件、库、编译产物
  "exe", "dll", "so", "dylib", "o", "a", "lib", "bin", "class", "pyc", "pyo", "pyd", "wasm", "node", "elf", "sys",
  // 字体
  "ttf", "otf", "woff", "woff2", "eot",
  // 数据、模型
  "db", "sqlite", "sqlite3", "mdb", "parquet", "feather", "arrow", "npy", "npz", "pkl", "pickle", "pt", "pth", "ckpt", "safetensors", "onnx", "h5", "hdf5", "tfrecord", "joblib", "gguf",
]);

/** 按扩展名判断一定不是文本（不区分大小写）；没有扩展名的返回 false */
export function isBinaryName(name: string): boolean {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return false;
  return BINARY.has(name.slice(dot + 1).toLowerCase());
}
