/**
 * 手机发给 AI 的图片：长边缩到 1568 像素以内（Claude 看图的有效上限，也省流量），
 * 再编码成 JPEG；本来就小、格式也合适的原样发。
 */

/** 长边最多多少像素 */
const MAX_SIDE = 1568;
/** 原样发的上限（字节） */
const KEEP_BYTES = 1.5 * 1024 * 1024;
/** 电脑端接受的类型 */
const ACCEPTED = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/** 读出图片的宽高并按 EXIF 方向转正（不支持的浏览器退回 <img>） */
async function decode(file: Blob): Promise<{ source: CanvasImageSource; width: number; height: number; close: () => void }> {
  if ("createImageBitmap" in window) {
    try {
      const bitmap = await createImageBitmap(file, { imageOrientation: "from-image" });
      return { source: bitmap, width: bitmap.width, height: bitmap.height, close: () => bitmap.close() };
    } catch {
      // HEIC 等浏览器解不了的格式：下面用 <img> 再试一次
    }
  }
  const url = URL.createObjectURL(file);
  try {
    const img = new Image();
    img.src = url;
    await img.decode();
    return { source: img, width: img.naturalWidth, height: img.naturalHeight, close: () => URL.revokeObjectURL(url) };
  } catch (e) {
    URL.revokeObjectURL(url);
    throw e;
  }
}

/**
 * 准备要发的图片。
 * @param file 用户选的图片
 * @returns 要上传的数据和类型
 */
export async function prepareImage(file: File): Promise<Blob> {
  const image = await decode(file);
  try {
    const small = Math.max(image.width, image.height) <= MAX_SIDE;
    if (small && file.size <= KEEP_BYTES && ACCEPTED.includes(file.type)) return file;
    const scale = Math.min(1, MAX_SIDE / Math.max(image.width, image.height));
    const canvas = document.createElement("canvas");
    canvas.width = Math.max(1, Math.round(image.width * scale));
    canvas.height = Math.max(1, Math.round(image.height * scale));
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("canvas");
    // 透明部分铺白底再转 JPEG
    ctx.fillStyle = "#fff";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(image.source, 0, 0, canvas.width, canvas.height);
    const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/jpeg", 0.85));
    if (!blob) throw new Error("encode");
    return blob;
  } finally {
    image.close();
  }
}
