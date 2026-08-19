import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
	return twMerge(clsx(inputs));
}

export function displayTitle(chat: { title: string; alias?: string | null }): string {
	const alias = chat.alias?.trim();
	return alias || chat.title;
}

export type TextSegment = { text: string; href?: string };

const PLAIN_URL = /https?:\/\/[^\s<>"'）)\]]+/gi;

export function isSafeHttpUrl(raw: string): boolean {
	const url = raw.trim();
	if (!url || /[\s\u0000-\u001f]/.test(url)) return false;
	const lower = url.toLowerCase();
	if (!lower.startsWith('https://') && !lower.startsWith('http://')) return false;
	const rest = lower.startsWith('https://') ? url.slice(8) : url.slice(7);
	return rest.length > 0 && !rest.startsWith('/');
}

export function messageSegments(
	text: string,
	links: { offset: number; length: number; url: string }[] = []
): TextSegment[] {
	const entity = applyEntityLinks(text, links);
	return entity.flatMap((seg) => (seg.href ? [seg] : linkifyPlain(seg.text)));
}

function applyEntityLinks(
	text: string,
	links: { offset: number; length: number; url: string }[]
): TextSegment[] {
	const sorted = [...links]
		.filter((link) => link.length > 0 && isSafeHttpUrl(link.url))
		.sort((a, b) => a.offset - b.offset);
	const out: TextSegment[] = [];
	let cursor = 0;
	for (const link of sorted) {
		const start = Math.max(0, link.offset);
		const end = Math.min(text.length, start + link.length);
		if (start < cursor || start >= text.length || end <= start) continue;
		if (start > cursor) out.push({ text: text.slice(cursor, start) });
		out.push({ text: text.slice(start, end), href: link.url.trim() });
		cursor = end;
	}
	if (cursor < text.length) out.push({ text: text.slice(cursor) });
	return out.length > 0 ? out : [{ text }];
}

function linkifyPlain(text: string): TextSegment[] {
	const out: TextSegment[] = [];
	let cursor = 0;
	for (const match of text.matchAll(PLAIN_URL)) {
		const raw = match[0];
		const index = match.index ?? 0;
		const href = trimUrlPunct(raw);
		if (!isSafeHttpUrl(href)) continue;
		if (index > cursor) out.push({ text: text.slice(cursor, index) });
		out.push({ text: href, href });
		cursor = index + href.length;
	}
	if (cursor < text.length) out.push({ text: text.slice(cursor) });
	return out.length > 0 ? out : [{ text }];
}

function trimUrlPunct(url: string): string {
	return url.replace(/[.,;:!?，。；：！？]+$/u, '');
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type WithoutChild<T> = T extends { child?: any } ? Omit<T, "child"> : T;
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type WithoutChildren<T> = T extends { children?: any } ? Omit<T, "children"> : T;
export type WithoutChildrenOrChild<T> = WithoutChildren<WithoutChild<T>>;
export type WithElementRef<T, U extends HTMLElement = HTMLElement> = T & { ref?: U | null };
