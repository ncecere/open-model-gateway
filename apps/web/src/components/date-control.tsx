import { DatePicker } from "./ui/date-picker/date-picker";
import { parseISODate, toISODate } from "./ui/calendar/calendar";
/** Calendar cells represent named UTC report days, not instants. The library's
 * local-calendar parsing/serialization is paired; never convert them through toISOString. */
export function DateControl({ value, onChange, id, disabled, name }: { value: string; onChange: (value: string) => void; id: string; disabled?: boolean; name: string }) { return <DatePicker id={id} name={name} block value={parseISODate(value)} onValueChange={day => onChange(day ? toISODate(day) : "")} disabled={disabled} captionLayout="dropdown" />; }
