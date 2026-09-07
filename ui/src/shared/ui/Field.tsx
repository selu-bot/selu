import {
  createContext, forwardRef, useContext, useId,
  type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes, type TextareaHTMLAttributes,
} from 'react'
import { cx, mergeIds } from './utils'

type FieldContextValue = { controlId: string; descriptionId?: string; errorId?: string; invalid: boolean }
const FieldContext = createContext<FieldContextValue | null>(null)

export type FieldProps = {
  children: ReactNode
  label: ReactNode
  id?: string
  hint?: ReactNode
  error?: ReactNode
  optional?: ReactNode
  className?: string
}

export function Field({ children, label, id, hint, error, optional, className }: FieldProps) {
  const generatedId = useId()
  const controlId = id ?? generatedId
  const descriptionId = hint ? `${controlId}-hint` : undefined
  const errorId = error ? `${controlId}-error` : undefined
  return <FieldContext.Provider value={{ controlId, descriptionId, errorId, invalid: Boolean(error) }}>
    <div className={cx('selu-ui-field', Boolean(error) && 'has-error', className)}>
      <label htmlFor={controlId} className="selu-ui-field-label">
        <span>{label}</span>{optional && <span className="selu-ui-field-optional">{optional}</span>}
      </label>
      {children}
      {hint && <div id={descriptionId} className="selu-ui-field-hint">{hint}</div>}
      {error && <div id={errorId} className="selu-ui-field-error" role="alert">{error}</div>}
    </div>
  </FieldContext.Provider>
}

function useFieldAttributes(id: string | undefined, describedBy: string | undefined, invalid: boolean | undefined) {
  const field = useContext(FieldContext)
  return {
    id: id ?? field?.controlId,
    'aria-describedby': mergeIds(describedBy, field?.descriptionId, field?.errorId),
    'aria-invalid': invalid ?? (field?.invalid || undefined),
  }
}

export type InputProps = InputHTMLAttributes<HTMLInputElement>
export const Input = forwardRef<HTMLInputElement, InputProps>(function Input(
  { className, id, 'aria-describedby': describedBy, 'aria-invalid': invalid, ...props }, ref,
) {
  const field = useFieldAttributes(id, describedBy, invalid == null ? undefined : invalid === true || invalid === 'true')
  return <input {...props} {...field} ref={ref} className={cx('selu-ui-control', className)} />
})

export type SelectProps = SelectHTMLAttributes<HTMLSelectElement>
export const Select = forwardRef<HTMLSelectElement, SelectProps>(function Select(
  { className, id, 'aria-describedby': describedBy, 'aria-invalid': invalid, ...props }, ref,
) {
  const field = useFieldAttributes(id, describedBy, invalid == null ? undefined : invalid === true || invalid === 'true')
  return <select {...props} {...field} ref={ref} className={cx('selu-ui-control', 'selu-ui-select', className)} />
})

export type TextareaProps = TextareaHTMLAttributes<HTMLTextAreaElement>
export const Textarea = forwardRef<HTMLTextAreaElement, TextareaProps>(function Textarea(
  { className, id, 'aria-describedby': describedBy, 'aria-invalid': invalid, ...props }, ref,
) {
  const field = useFieldAttributes(id, describedBy, invalid == null ? undefined : invalid === true || invalid === 'true')
  return <textarea {...props} {...field} ref={ref} className={cx('selu-ui-control', 'selu-ui-textarea', className)} />
})
