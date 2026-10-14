/**
 * SHA-256 计算 Worker。
 *
 * 为什么不是「读整个文件丢给 crypto.subtle.digest」：
 * Web Crypto 的 digest 是一次性接口，没有流式 API。20MB 的单文件上限下整块读没问题，
 * 但管理员把上限调大之后，整块读一个几百 MB 的文件会把 Worker 的内存顶满。
 * 所以分两条路走，产出的哈希值与标准 SHA-256 完全一致（服务端预检才能对上）：
 *   - ≤ 32MB：一次读完交给原生 digest，最快；
 *   - > 32MB：增量实现 + 4MB 分片读取，内存占用与文件大小无关。
 *
 * 纯函数（Sha256 / chunkOffsets）导出是为了能在 node 里对着标准实现做测试，
 * Worker 入口本身对这些导出没有任何依赖。
 */

declare const self: DedicatedWorkerGlobalScope;

/** 一次性读取的上限：再大就换成增量实现，避免把文件整个读进 Worker 内存。 */
const ONE_SHOT_LIMIT = 32 * 1024 * 1024;
/** 增量路线的分片大小。4MB 是读放大与消息开销之间的折中。 */
const CHUNK_SIZE = 4 * 1024 * 1024;

export interface HashRequest {
  type: "hash";
  id: string;
  file: File;
}

export type HashResponse =
  | { type: "progress"; id: string; loaded: number; total: number }
  | { type: "done"; id: string; sha256: string }
  | { type: "error"; id: string; message: string };

/** 分片切分：返回每片的 [起, 止) 偏移。纯函数，单测直接对着它跑。 */
export function chunkOffsets(size: number, chunkSize = CHUNK_SIZE): Array<[number, number]> {
  const chunks: Array<[number, number]> = [];
  if (!Number.isFinite(size) || size <= 0 || chunkSize <= 0) return chunks;
  for (let start = 0; start < size; start += chunkSize) {
    chunks.push([start, Math.min(start + chunkSize, size)]);
  }
  return chunks;
}

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

function rotr(value: number, bits: number): number {
  return ((value >>> bits) | (value << (32 - bits))) >>> 0;
}

/** 增量 SHA-256：update 可以喂任意大小的分片，digest 只在最后调用一次。 */
export class Sha256 {
  private h = new Uint32Array([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
  ]);
  private buffer = new Uint8Array(64);
  private buffered = 0;
  private bytesHashed = 0;
  private schedule = new Uint32Array(64);

  update(chunk: Uint8Array): void {
    this.push(chunk);
  }

  private push(bytes: Uint8Array): void {
    this.bytesHashed += bytes.length;
    let offset = 0;
    // 先把上次残留的半块补齐
    if (this.buffered > 0) {
      const need = 64 - this.buffered;
      const take = Math.min(need, bytes.length);
      this.buffer.set(bytes.subarray(0, take), this.buffered);
      this.buffered += take;
      offset = take;
      if (this.buffered === 64) {
        this.compress(this.buffer, 0);
        this.buffered = 0;
      } else {
        return; // 还没凑满一块
      }
    }
    // 整块直接原地压缩，不进缓冲区
    while (offset + 64 <= bytes.length) {
      this.compress(bytes, offset);
      offset += 64;
    }
    // 尾巴留着等下一片
    if (offset < bytes.length) {
      this.buffer.set(bytes.subarray(offset), 0);
      this.buffered = bytes.length - offset;
    }
  }

  private compress(block: Uint8Array, offset: number): void {
    const w = this.schedule;
    for (let i = 0; i < 16; i += 1) {
      const j = offset + i * 4;
      w[i] = ((block[j]! << 24) | (block[j + 1]! << 16) | (block[j + 2]! << 8) | block[j + 3]!) >>> 0;
    }
    for (let i = 16; i < 64; i += 1) {
      const x = w[i - 15]!;
      const y = w[i - 2]!;
      const s0 = rotr(x, 7) ^ rotr(x, 18) ^ (x >>> 3);
      const s1 = rotr(y, 17) ^ rotr(y, 19) ^ (y >>> 10);
      w[i] = (w[i - 16]! + s0 + w[i - 7]! + s1) >>> 0;
    }

    let a = this.h[0]!;
    let b = this.h[1]!;
    let c = this.h[2]!;
    let d = this.h[3]!;
    let e = this.h[4]!;
    let f = this.h[5]!;
    let g = this.h[6]!;
    let hh = this.h[7]!;

    for (let i = 0; i < 64; i += 1) {
      const s1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
      const ch = (e & f) ^ (~e & g);
      const temp1 = (hh + s1 + ch + K[i]! + w[i]!) >>> 0;
      const s0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const temp2 = (s0 + maj) >>> 0;

      hh = g;
      g = f;
      f = e;
      e = (d + temp1) >>> 0;
      d = c;
      c = b;
      b = a;
      a = (temp1 + temp2) >>> 0;
    }

    this.h[0] = (this.h[0]! + a) >>> 0;
    this.h[1] = (this.h[1]! + b) >>> 0;
    this.h[2] = (this.h[2]! + c) >>> 0;
    this.h[3] = (this.h[3]! + d) >>> 0;
    this.h[4] = (this.h[4]! + e) >>> 0;
    this.h[5] = (this.h[5]! + f) >>> 0;
    this.h[6] = (this.h[6]! + g) >>> 0;
    this.h[7] = (this.h[7]! + hh) >>> 0;
  }

  digest(): string {
    const bitLengthHi = Math.floor(this.bytesHashed / 0x20000000);
    const bitLengthLo = (this.bytesHashed << 3) >>> 0;
    // 补齐块 = 余数 + 0x80 + 零填充 + 8 字节大端比特长度；装不下就再加一整块。
    // 直接 compress，不能走 push——push 把入参当流式输入，而余数已经在内层缓冲里，
    // 再喂一遍会把它当新数据重新缓冲，长度字段就被挤到下一个（不存在的）块里。
    const size = this.buffered <= 55 ? 64 : 128;
    const tail = new Uint8Array(size);
    tail.set(this.buffer.subarray(0, this.buffered), 0);
    tail[this.buffered] = 0x80;
    const view = new DataView(tail.buffer);
    view.setUint32(size - 8, bitLengthHi);
    view.setUint32(size - 4, bitLengthLo);
    for (let offset = 0; offset < size; offset += 64) this.compress(tail, offset);
    this.buffered = 0;

    let out = "";
    for (let i = 0; i < 8; i += 1) out += this.h[i]!.toString(16).padStart(8, "0");
    return out;
  }
}

function toHex(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let out = "";
  for (let i = 0; i < bytes.length; i += 1) out += bytes[i]!.toString(16).padStart(2, "0");
  return out;
}

/** 对任意 Blob 求标准 SHA-256。导出用于测试与降级路径复用。 */
export async function sha256OfBlob(
  blob: Blob,
  onProgress?: (loaded: number, total: number) => void,
): Promise<string> {
  if (blob.size <= ONE_SHOT_LIMIT) {
    const buffer = await blob.arrayBuffer();
    onProgress?.(blob.size, blob.size);
    const digest = await crypto.subtle.digest("SHA-256", buffer);
    return toHex(digest);
  }

  const hasher = new Sha256();
  let loaded = 0;
  for (const [start, end] of chunkOffsets(blob.size)) {
    const bytes = new Uint8Array(await blob.slice(start, end).arrayBuffer());
    hasher.update(bytes);
    loaded = end;
    onProgress?.(loaded, blob.size);
  }
  return hasher.digest();
}

function reply(message: HashResponse): void {
  self.postMessage(message);
}

self.addEventListener("message", (event: MessageEvent<HashRequest>) => {
  const task = event.data;
  if (!task || task.type !== "hash") return;
  void (async () => {
    try {
      const sha256 = await sha256OfBlob(task.file, (loaded, total) =>
        reply({ type: "progress", id: task.id, loaded, total }),
      );
      reply({ type: "done", id: task.id, sha256 });
    } catch (err) {
      reply({
        type: "error",
        id: task.id,
        message: err instanceof Error ? err.message : "哈希计算失败",
      });
    }
  })();
});
