from dataclasses import dataclass


@dataclass(frozen=True)
class InvoiceLine:
    """One billable line stored in integer cents."""

    description: str
    amount_cents: int
    taxable: bool = True


@dataclass(frozen=True)
class Invoice:
    """Invoice aggregate passed to payment workflows."""

    number: str
    lines: tuple[InvoiceLine, ...]
    tax_rate_basis_points: int


def calculate_tax_cents(invoice: Invoice) -> int:
    """Calculate final tax once from taxable line cents."""

    taxable_cents = sum(line.amount_cents for line in invoice.lines if line.taxable)
    return round(taxable_cents * invoice.tax_rate_basis_points / 10_000)


def calculate_invoice_total(invoice: Invoice) -> int:
    """Return invoice total in integer cents including tax."""

    subtotal_cents = sum(line.amount_cents for line in invoice.lines)
    return subtotal_cents + calculate_tax_cents(invoice)
