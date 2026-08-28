export type RateSample = {
	bps: number;
	etaSecs: number | null;
};

type Prev = {
	bytes: number;
	at: number;
	ema: number | null;
};

const ALPHA = 0.35;
const MIN_DT_MS = 80;
const STALE_MS = 2500;
const MIN_SHOW_BPS = 1024;

/** 按进度事件估瞬时速率；暂停或换文件会丢掉样本。 */
export class DownloadRateTracker {
	#prev = new Map<string, Prev>();

	update(
		items: { fileId: string; bytes: string; total: string | null }[],
		paused: boolean,
		now = Date.now()
	): Map<string, RateSample> {
		if (paused) {
			this.#prev.clear();
			return new Map();
		}
		const live = new Set(items.map((item) => item.fileId));
		for (const id of this.#prev.keys()) {
			if (!live.has(id)) this.#prev.delete(id);
		}
		const out = new Map<string, RateSample>();
		for (const item of items) {
			const bytes = Number(item.bytes);
			const total = item.total == null ? null : Number(item.total);
			if (!Number.isFinite(bytes) || bytes < 0) continue;
			const sample = this.#step(item.fileId, bytes, now);
			if (sample == null || sample < MIN_SHOW_BPS) continue;
			const remain =
				total != null && Number.isFinite(total) && total > bytes ? total - bytes : null;
			const etaSecs =
				remain != null && remain > 0 ? remain / sample : null;
			out.set(item.fileId, {
				bps: sample,
				etaSecs: etaSecs != null && etaSecs >= 1 && etaSecs < 24 * 3600 ? etaSecs : null
			});
		}
		return out;
	}

	#step(fileId: string, bytes: number, now: number): number | null {
		const prev = this.#prev.get(fileId);
		if (!prev || bytes < prev.bytes) {
			this.#prev.set(fileId, { bytes, at: now, ema: null });
			return null;
		}
		const dt = now - prev.at;
		if (dt < MIN_DT_MS) return prev.ema;
		const instant = dt > 0 ? ((bytes - prev.bytes) * 1000) / dt : 0;
		const ema =
			prev.ema == null
				? dt >= STALE_MS
					? null
					: instant
				: dt >= STALE_MS && instant === 0
					? prev.ema * 0.4
					: prev.ema * (1 - ALPHA) + instant * ALPHA;
		this.#prev.set(fileId, { bytes, at: now, ema });
		return ema;
	}
}

export function formatRate(bps: number): string {
	if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(1)} KB/s`;
	if (bps < 1024 * 1024 * 1024) return `${(bps / 1024 / 1024).toFixed(1)} MB/s`;
	return `${(bps / 1024 / 1024 / 1024).toFixed(2)} GB/s`;
}

export function formatEta(secs: number): string {
	if (secs < 60) return `约 ${Math.max(1, Math.round(secs))} 秒`;
	if (secs < 3600) return `约 ${Math.max(1, Math.round(secs / 60))} 分钟`;
	const hours = secs / 3600;
	if (hours < 10) return `约 ${hours.toFixed(1)} 小时`;
	return `约 ${Math.round(hours)} 小时`;
}

export function formatRateLine(sample: RateSample): string {
	const rate = formatRate(sample.bps);
	return sample.etaSecs != null ? `${rate} · ${formatEta(sample.etaSecs)}` : rate;
}
