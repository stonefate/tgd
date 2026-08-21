import { convertFileSrc } from '@tauri-apps/api/core';

import { httpPath, isTauri } from '$lib/runtime';

export function mediaSrc(path: string): string | null {
	if (!path) return null;
	if (isTauri()) {
		try {
			return convertFileSrc(path);
		} catch {
			return null;
		}
	}
	return `${httpPath('/api/media')}?path=${encodeURIComponent(path)}`;
}

/** 飞牛网格/列表用缩略图；桌面仍走本地原图。 */
export function mediaThumbSrc(path: string): string | null {
	if (!path) return null;
	if (isTauri()) return mediaSrc(path);
	return `${httpPath('/api/media/thumb')}?path=${encodeURIComponent(path)}`;
}

export function fallbackMediaSrc(target: EventTarget | null, full: string | null) {
	if (!full || !(target instanceof HTMLImageElement) || target.dataset.full === '1') return;
	target.dataset.full = '1';
	target.src = full;
}
