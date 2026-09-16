import type { ApiEnvelope, ApiFailure, DatasetStats, HealthResponse, ReadyResponse } from './types'

const TOKEN_KEY = 'lhr.studio.api-token'

export class ApiError extends Error {
  readonly status: number
  readonly requestId?: number

  constructor(message: string, status: number, requestId?: number) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.requestId = requestId
  }
}

export function getApiToken(): string {
  return sessionStorage.getItem(TOKEN_KEY) ?? ''
}

export function setApiToken(token: string): void {
  const value = token.trim()
  if (value) sessionStorage.setItem(TOKEN_KEY, value)
  else sessionStorage.removeItem(TOKEN_KEY)
}

async function request<T>(path: string, init: RequestInit = {}, authenticated = true): Promise<T> {
  const headers = new Headers(init.headers)
  const token = getApiToken()

  if (init.body && !headers.has('content-type')) headers.set('content-type', 'application/json')
  if (authenticated && token) headers.set('authorization', `Bearer ${token}`)

  const response = await fetch(path, {
    ...init,
    headers,
    credentials: 'same-origin',
  })

  const contentType = response.headers.get('content-type') ?? ''
  const payload = contentType.includes('application/json') ? await response.json() : await response.text()

  if (!response.ok) {
    const failure = typeof payload === 'object' && payload !== null ? (payload as ApiFailure) : undefined
    throw new ApiError(failure?.error ?? `Request failed with HTTP ${response.status}`, response.status, failure?.request_id)
  }

  return payload as T
}

export const api = {
  health: () => request<HealthResponse>('/healthz', {}, false),
  ready: () => request<ReadyResponse>('/readyz', {}, false),
  stats: async () => (await request<ApiEnvelope<DatasetStats>>('/v1/stats')).result,
}
