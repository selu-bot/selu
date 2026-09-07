import { apiRequest } from '../../api'
export type FeedbackInput = { category: 'bug' | 'idea' | 'question' | 'other'; title?: string; description: string }
export type FeedbackReceipt = { status: string; issue_number: number; issue_url: string }
export const feedbackApi = {
  submit: (input: FeedbackInput) => apiRequest<FeedbackReceipt>('/api/v1/feedback', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(input) }),
}
