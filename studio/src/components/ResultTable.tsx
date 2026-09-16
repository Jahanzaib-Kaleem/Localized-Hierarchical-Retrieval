import type { QueryResponse } from '../api/types'
import { numberFormat } from './format'
import { EmptyState } from './ui'

export function ResultTable({ result }: { result?: QueryResponse }) {
  if (!result) return <EmptyState>Run a bounded query to inspect rows.</EmptyState>
  if (!result.rows.length) return <EmptyState>No rows matched this query.</EmptyState>
  const columns = result.rows[0].values.map((value) => value.column)
  return (
    <div className="table-wrap table-wrap--results">
      <table className="data-table data-table--results">
        <thead><tr><th>row_id</th>{columns.map((column) => <th key={column}>{column}</th>)}</tr></thead>
        <tbody>
          {result.rows.map((row) => {
            const values = new Map(row.values.map((value) => [value.column, value.value]))
            return <tr key={row.row_id}><td className="mono data-table__primary">{numberFormat.format(row.row_id)}</td>{columns.map((column) => <td key={column} title={values.get(column) ?? 'NULL'}>{values.get(column) ?? <span className="null-value">NULL</span>}</td>)}</tr>
          })}
        </tbody>
      </table>
    </div>
  )
}
