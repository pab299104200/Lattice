from taskflow.billing import Invoice, InvoiceLine, calculate_invoice_total


def test_calculate_invoice_total_adds_tax_once() -> None:
    invoice = Invoice(
        number="INV-100",
        lines=(InvoiceLine("service", 10_000), InvoiceLine("credit", -500, False)),
        tax_rate_basis_points=750,
    )

    assert calculate_invoice_total(invoice) == 10_250
