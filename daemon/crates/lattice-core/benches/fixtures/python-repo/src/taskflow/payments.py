from dataclasses import dataclass

from .billing import Invoice, calculate_invoice_total


@dataclass(frozen=True)
class PaymentRequest:
    """A gateway request built from an invoice total."""

    invoice_number: str
    amount_cents: int


class PaymentGateway:
    """Boundary object for external payment submission."""

    def charge(self, request: PaymentRequest) -> str:
        if request.amount_cents <= 0:
            raise ValueError("Payment amount must be positive")
        return f"paid:{request.invoice_number}"


def build_payment_request(invoice: Invoice) -> PaymentRequest:
    """Build the request using calculate_invoice_total as server truth."""

    return PaymentRequest(
        invoice_number=invoice.number,
        amount_cents=calculate_invoice_total(invoice),
    )


def process_payment(invoice: Invoice, gateway: PaymentGateway) -> str:
    """Charge an invoice through a provided gateway."""

    return gateway.charge(build_payment_request(invoice))
