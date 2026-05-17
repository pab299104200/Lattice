from taskflow.billing import Invoice, InvoiceLine
from taskflow.payments import PaymentGateway, process_payment


def test_process_payment_uses_invoice_total() -> None:
    invoice = Invoice("INV-101", (InvoiceLine("service", 2_000),), 500)

    assert process_payment(invoice, PaymentGateway()) == "paid:INV-101"
