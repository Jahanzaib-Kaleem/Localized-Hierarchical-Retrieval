import type { ReactNode } from 'react'

export function Panel({ title, eyebrow, action, children, className = '' }: { title: string; eyebrow?: string; action?: ReactNode; children: ReactNode; className?: string }) {
  return <section className={`panel ${className}`.trim()}><header className="panel__header"><div className="stack stack--xs">{eyebrow ? <span className="eyebrow">{eyebrow}</span> : null}<h2 className="panel__title">{title}</h2></div>{action ? <div className="panel__action">{action}</div> : null}</header><div className="panel__body">{children}</div></section>
}

export function Metric({ label, value, detail }: { label: string; value: ReactNode; detail?: ReactNode }) {
  return <div className="metric"><span className="metric__label">{label}</span><strong className="metric__value">{value}</strong>{detail ? <span className="metric__detail">{detail}</span> : null}</div>
}

export function StatusMark({ active = true }: { active?: boolean }) { return <span className="status-mark" data-active={active ? 'true' : 'false'} aria-hidden="true" /> }
export function EmptyState({ children }: { children: ReactNode }) { return <div className="empty-state">{children}</div> }
export function Notice({ title, children }: { title: string; children?: ReactNode }) { return <div className="notice"><strong>{title}</strong>{children ? <span>{children}</span> : null}</div> }
export function PageHeader({ eyebrow, title, description, action }: { eyebrow: string; title: string; description: string; action?: ReactNode }) {
  return <header className="page-header"><div className="stack stack--xs"><span className="eyebrow">{eyebrow}</span><h1 className="page-title">{title}</h1><p className="page-description">{description}</p></div>{action}</header>
}
export function ActionResult({ value }: { value: unknown }) { return <pre className="result-block">{JSON.stringify(value, null, 2)}</pre> }
