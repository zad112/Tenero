# Coins are stored everywhere as WHOLE NUMBERS of the smallest unit, so the
# arithmetic is exact (no floating point rounding). With DECIMALS = 4 the
# smallest unit is 0.0001 coins, and 1 coin = 10,000 units.
#
# Use to_units() to turn what a person types into units, and fmt() to show
# units as coins. Everything inside the chain (amounts, fees, rewards, supply)
# is in units.
#
# Changing DECIMALS changes what every stored number means, so after changing
# it delete chain.json and mempool.json.

from decimal import Decimal, InvalidOperation

DECIMALS = 4
UNIT = 10 ** DECIMALS


def to_units(value):
    """Coins (text like '1.2345', an int, or a Decimal) -> whole units.
    Raises ValueError for junk or for more than DECIMALS decimal places."""
    if isinstance(value, float):
        value = repr(value)
    try:
        d = Decimal(str(value).strip())
    except InvalidOperation:
        raise ValueError(f"'{value}' is not a valid number")
    if not d.is_finite():
        raise ValueError(f"'{value}' is not a valid number")
    scaled = d * UNIT
    if scaled != scaled.to_integral_value():
        raise ValueError(f"amounts can have at most {DECIMALS} decimal places")
    return int(scaled)


def fmt(units):
    """Whole units -> text in coins, always with DECIMALS decimal places."""
    sign = "-" if units < 0 else ""
    u = abs(int(units))
    return f"{sign}{u // UNIT}.{u % UNIT:0{DECIMALS}d}"
