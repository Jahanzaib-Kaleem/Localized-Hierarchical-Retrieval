export const numberFormat = new Intl.NumberFormat('en-US')

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const value = bytes / 1024 ** index
  return `${value >= 100 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`
}

export function formatMicros(value: number): string {
  if (value < 1000) return `${numberFormat.format(Math.round(value))} µs`
  if (value < 1_000_000) return `${(value / 1000).toFixed(value < 10_000 ? 2 : 1)} ms`
  return `${(value / 1_000_000).toFixed(2)} s`
}

export function formatPercent(value: number): string { return `${(value * 100).toFixed(1)}%` }
