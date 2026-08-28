import type {
	AppError,
	AppInfo,
	ChatDownloadTypes,
	ChatItem,
	DownloadItem,
	DownloadProgress,
	DownloadUsage,
	IlinkStatus,
	LogEntry,
	MessagePage,
	TelegramStatus,
	ProxyConfig
} from '$lib/bindings';
import { mediaSrc } from '$lib/media';
import { httpPath } from '$lib/runtime';

type Envelope<T> = { status: 'ok'; data: T } | { status: 'error'; error: AppError };

async function rpc<T>(path: string, init?: RequestInit): Promise<Envelope<T>> {
	const response = await fetch(httpPath(path), {
		credentials: 'include',
		...init,
		headers: {
			...(init?.body ? { 'content-type': 'application/json' } : {}),
			...init?.headers
		}
	});
	const json = (await response.json().catch(() => null)) as Envelope<T> | null;
	if (json && (json.status === 'ok' || json.status === 'error')) {
		return json;
	}
	throw new Error(response.ok ? '服务返回无效数据' : `无法连接 tgd 服务（${response.status}）`);
}

async function data<T>(path: string, init?: RequestInit): Promise<T> {
	const result = await rpc<T>(path, init);
	if (result.status === 'ok') return result.data;
	throw new Error(result.error.message || '操作失败');
}

let eventSource: EventSource | null = null;
let eventSourceRefs = 0;

function sharedEvents(): EventSource {
	if (!eventSource || eventSource.readyState === EventSource.CLOSED) {
		eventSource = new EventSource(httpPath('/api/events'), { withCredentials: true });
	}
	return eventSource;
}

function listenEvent<T>(name: string, cb: (event: { payload: T }) => void): Promise<() => void> {
	const source = sharedEvents();
	eventSourceRefs += 1;
	const handler = (event: MessageEvent) => {
		try {
			cb({ payload: JSON.parse(event.data) as T });
		} catch {
			// 忽略无法解析的心跳
		}
	};
	source.addEventListener(name, handler as EventListener);
	return Promise.resolve(() => {
		source.removeEventListener(name, handler as EventListener);
		eventSourceRefs -= 1;
		if (eventSourceRefs <= 0) {
			eventSourceRefs = 0;
			source.close();
			if (eventSource === source) eventSource = null;
		}
	});
}

export const httpCommands = {
	getAppInfo: () => data<AppInfo>('/api/app-info'),
	getRecentLogs: () => data<LogEntry[]>('/api/logs'),
	getTelegramStatus: () => rpc<TelegramStatus>('/api/telegram/status'),
	connectTelegram: () => rpc<TelegramStatus>('/api/telegram/connect', { method: 'POST' }),
	requestLoginCode: (phone: string) =>
		rpc<TelegramStatus>('/api/telegram/request-code', {
			method: 'POST',
			body: JSON.stringify({ phone })
		}),
	submitLoginCode: (code: string) =>
		rpc<TelegramStatus>('/api/telegram/submit-code', {
			method: 'POST',
			body: JSON.stringify({ code })
		}),
	submitPassword: (password: string) =>
		rpc<TelegramStatus>('/api/telegram/submit-password', {
			method: 'POST',
			body: JSON.stringify({ password })
		}),
	logout: () => rpc<TelegramStatus>('/api/telegram/logout', { method: 'POST' }),
	listChats: (refresh: boolean) =>
		rpc<ChatItem[]>(`/api/chats${refresh ? '?refresh=true' : ''}`),
	setChatWatched: (chatId: string, watched: boolean) =>
		rpc<boolean>('/api/chats/watched', {
			method: 'POST',
			body: JSON.stringify({ chatId, watched })
		}),
	setGuestWatch: (enabled: boolean, query: string) =>
		rpc<TelegramStatus>('/api/settings/guest-watch', {
			method: 'POST',
			body: JSON.stringify({ enabled, query })
		}),
	setChatDownloadTypes: (chatId: string, types: ChatDownloadTypes) =>
		rpc<ChatDownloadTypes>('/api/chats/types', {
			method: 'POST',
			body: JSON.stringify({ chatId, types })
		}),
	setBackfillDays: (days: number) =>
		rpc<number>('/api/settings/backfill-days', {
			method: 'POST',
			body: JSON.stringify({ days })
		}),
	setChatBackfillDays: (chatId: string, days: number | null) =>
		rpc<number>('/api/chats/backfill-days', {
			method: 'POST',
			body: JSON.stringify({ chatId, days })
		}),
	setChatAlias: (chatId: string, alias: string | null) =>
		rpc<string | null>('/api/chats/alias', {
			method: 'POST',
			body: JSON.stringify({ chatId, alias })
		}),
	setShowMedia: (enabled: boolean) =>
		rpc<boolean>('/api/settings/show-media', {
			method: 'POST',
			body: JSON.stringify({ enabled })
		}),
	setProxy: (config: ProxyConfig) =>
		rpc<TelegramStatus>('/api/settings/proxy', {
			method: 'POST',
			body: JSON.stringify(config)
		}),
	setDownloadDir: (dir: string) =>
		rpc<TelegramStatus>('/api/settings/download-dir', {
			method: 'POST',
			body: JSON.stringify({ dir })
		}),
	getDownloadDirOptions: () =>
		data<{ current: string; available: string[] }>('/api/settings/download-dirs'),
	setAutostart: (_enabled: boolean) => rpc<boolean>('/api/settings/autostart', { method: 'POST' }),
	setDownloadConcurrency: (n: number) =>
		rpc<number>('/api/settings/download-concurrency', {
			method: 'POST',
			body: JSON.stringify({ n })
		}),
	setMinMediaMb: (n: number) =>
		rpc<number>('/api/settings/min-media-mb', {
			method: 'POST',
			body: JSON.stringify({ n })
		}),
	getDownloadStatus: () => data<DownloadProgress>('/api/downloads/status'),
	setDownloadPaused: (paused: boolean) =>
		rpc<boolean>('/api/settings/download-paused', {
			method: 'POST',
			body: JSON.stringify({ paused })
		}),
	getIlinkStatus: () => data<IlinkStatus>('/api/ilink/status'),
	ilinkStartLogin: () => rpc<IlinkStatus>('/api/ilink/login', { method: 'POST' }),
	ilinkLogout: () => rpc<IlinkStatus>('/api/ilink/logout', { method: 'POST' }),
	setIlinkNotifyEnabled: (enabled: boolean) =>
		rpc<IlinkStatus>('/api/ilink/enabled', {
			method: 'POST',
			body: JSON.stringify({ enabled })
		}),
	ilinkSendTest: () => rpc<IlinkStatus>('/api/ilink/test', { method: 'POST' }),
	cancelDownload: (fileId: string) =>
		rpc<boolean>('/api/downloads/cancel', {
			method: 'POST',
			body: JSON.stringify({ fileId })
		}),
	listDownloads: () => rpc<DownloadItem[]>('/api/downloads'),
	getDownloadUsage: () => rpc<DownloadUsage>('/api/downloads/usage'),
	listMessages: (
		chatId: string,
		query: string | null,
		beforeMessageId: number | null,
		limit: number | null
	) => {
		const params = new URLSearchParams({ chatId });
		if (query) params.set('query', query);
		if (beforeMessageId != null) params.set('beforeMessageId', String(beforeMessageId));
		if (limit != null) params.set('limit', String(limit));
		return rpc<MessagePage>(`/api/messages?${params}`);
	},
	searchMessages: (
		query: string,
		cursor: { dateUnix: string; chatId: string; messageId: number } | null,
		limit: number | null
	) => {
		const params = new URLSearchParams({ query });
		if (cursor) {
			params.set('dateUnix', cursor.dateUnix);
			params.set('chatId', cursor.chatId);
			params.set('messageId', String(cursor.messageId));
		}
		if (limit != null) params.set('limit', String(limit));
		return rpc<MessagePage>(`/api/search?${params}`);
	},
	clearChatMessages: (chatId: string) =>
		rpc<number>('/api/chats/clear-messages', {
			method: 'POST',
			body: JSON.stringify({ chatId })
		}),
	checkChatMedia: (chatId: string) =>
		rpc<boolean>('/api/chats/check-media', {
			method: 'POST',
			body: JSON.stringify({ chatId })
		}),
	redownloadMessageMedia: (chatId: string, messageId: number) =>
		rpc<boolean>('/api/messages/redownload', {
			method: 'POST',
			body: JSON.stringify({ chatId, messageId })
		}),
	openUrl: async (url: string) => {
		const result = await rpc<null>('/api/open-url', {
			method: 'POST',
			body: JSON.stringify({ url })
		});
		if (result.status === 'ok') window.open(url, '_blank', 'noopener');
		return result;
	},
	openPath: async (path: string) => {
		const src = mediaSrc(path);
		if (src) window.open(src, '_blank', 'noopener');
		return { status: 'ok' as const, data: null };
	},
	openDownloadDir: async () => {
		const result = await rpc<TelegramStatus>('/api/telegram/status');
		if (result.status === 'ok') {
			return { status: 'ok' as const, data: result.data.downloadDir };
		}
		return result;
	},
	pickDownloadDir: () => rpc<TelegramStatus>('/api/telegram/status'),
	showMainWindow: async () => {},
	hideMainWindow: async () => {},
	quitApp: async () => {}
};

export const httpEvents = {
	chatIngested: {
		listen: (cb: (event: { payload: { chatId: string } }) => void) =>
			listenEvent('chat-ingested', cb)
	},
	downloadProgress: {
		listen: (cb: (event: { payload: DownloadProgress }) => void) =>
			listenEvent('download-progress', cb)
	},
	telegramStatusChanged: {
		listen: (cb: (event: { payload: { connected: boolean; authorized: boolean } }) => void) =>
			listenEvent('telegram-status-changed', cb)
	},
	ilinkStatus: {
		listen: (cb: (event: { payload: IlinkStatus }) => void) => listenEvent('ilink-status', cb)
	}
};
