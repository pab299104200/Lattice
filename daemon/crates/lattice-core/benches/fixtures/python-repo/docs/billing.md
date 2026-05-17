# Billing Flow

`calculate_invoice_total` sums invoice lines and calls `calculate_tax_cents`.

`process_payment` builds a `PaymentRequest` from `build_payment_request` before charging `PaymentGateway`.
