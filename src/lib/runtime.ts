import { base } from '$app/paths';

export function isTauri(): boolean {
	return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

/** 飞牛网关前缀 `/app/tgd`；桌面和本机 Vite 为空。 */
export function httpPath(path: string): string {
	const suffix = path.startsWith('/') ? path : `/${path}`;
	if (!base) return suffix;
	return suffix === '/' ? `${base}/` : `${base}${suffix}`;
}
