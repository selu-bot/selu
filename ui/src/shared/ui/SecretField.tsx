import { Eye, EyeOff } from 'lucide-react'
import { forwardRef, useState, type InputHTMLAttributes } from 'react'
import { IconButton } from './Button'
import { Input } from './Field'
import { cx } from './utils'

export type SecretFieldProps = Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> & {
  showLabel: string
  hideLabel: string
}

export const SecretField = forwardRef<HTMLInputElement, SecretFieldProps>(function SecretField(
  { className, showLabel, hideLabel, ...props }, ref,
) {
  const [visible, setVisible] = useState(false)
  return <div className={cx('selu-ui-secret-field', className)}>
    <Input {...props} ref={ref} type={visible ? 'text' : 'password'} className="selu-ui-secret-input" />
    <IconButton
      label={visible ? hideLabel : showLabel}
      aria-pressed={visible}
      size="sm"
      onClick={() => setVisible((current) => !current)}
    >{visible ? <EyeOff /> : <Eye />}</IconButton>
  </div>
})
